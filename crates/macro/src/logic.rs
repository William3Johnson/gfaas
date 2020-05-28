use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use std::env;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::token::Paren;
use syn::{
    parenthesized, Block, ExprLit, FnArg, Ident, Lit, Pat, ReturnType, Token, Type, Visibility,
};

#[derive(Debug)]
pub struct GwasmFn {
    vis: Visibility,
    fn_token: Token![fn],
    ident: Ident,
    paren_token: Paren,
    args: Punctuated<FnArg, Token![,]>,
    ret: ReturnType,
    body: Box<Block>,
}

impl Parse for GwasmFn {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let content;
        Ok(GwasmFn {
            vis: input.parse()?,
            fn_token: input.parse()?,
            ident: input.parse()?,
            paren_token: parenthesized!(content in input),
            args: content.parse_terminated(FnArg::parse)?,
            ret: input.parse()?,
            body: input.parse()?,
        })
    }
}

fn validate_arg_type(ty: &Type) -> bool {
    match ty {
        Type::Array(arr) => validate_arg_type(&arr.elem),
        Type::Slice(slice) => validate_arg_type(&slice.elem),
        Type::Reference(r#ref) => validate_arg_type(&r#ref.elem),
        Type::Path(path) => {
            let path = &path.path;
            if let Some(ident) = path.get_ident() {
                ident.to_string() == "u8"
            } else {
                false
            }
        }
        _ => false,
    }
}

fn validate_extract_args(input: impl IntoIterator<Item = FnArg>) -> Vec<(Box<Pat>, Box<Type>)> {
    let mut args = vec![];
    for arg in input {
        let (pat, ty) = match arg {
            FnArg::Typed(arg) => {
                if arg.attrs.len() > 0 {
                    panic!("attributes around fn args are unsupported");
                }
                let pat = arg.pat;
                let ty = arg.ty;
                if !validate_arg_type(&ty) {
                    panic!("unsupported argument type");
                }
                (pat, ty)
            }
            _ => panic!("self params are unsupported"),
        };
        args.push((pat, ty));
    }
    args
}

#[derive(Debug)]
pub struct GwasmAttr {
    ident: Ident,
    eq_token: Token![=],
    value: ExprLit,
}

impl Parse for GwasmAttr {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        Ok(GwasmAttr {
            ident: input.parse()?,
            eq_token: input.parse()?,
            value: input.parse()?,
        })
    }
}

#[derive(Debug)]
pub struct GwasmAttrs(Punctuated<GwasmAttr, Token![,]>);

impl Parse for GwasmAttrs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        Ok(GwasmAttrs(input.parse_terminated(GwasmAttr::parse)?))
    }
}

#[derive(Debug, Default)]
struct GwasmParams {
    datadir: Option<String>,
    rpc_address: Option<String>,
    rpc_port: Option<u16>,
    net: Option<String>,
}

// TODO parse optional datadir, host ip, port and net from attributes
pub(super) fn remote_fn_impl(attrs: GwasmAttrs, f: GwasmFn, preserved: TokenStream) -> TokenStream {
    // Parse attributes
    let mut params = GwasmParams::default();
    for attr in attrs.0.into_iter() {
        let attr_str = attr.ident.to_string();
        match attr_str.as_str() {
            "datadir" => {
                let lit = attr.value.lit;
                match lit {
                    Lit::Str(s) => params.datadir.replace(s.value()),
                    x => panic!("invalid attribute value '{:#?}'", x),
                };
            }
            "rpc_address" => {
                let lit = attr.value.lit;
                match lit {
                    Lit::Str(s) => params.rpc_address.replace(s.value()),
                    x => panic!("invalid attribute value '{:#?}'", x),
                };
            }
            "rpc_port" => {
                let lit = attr.value.lit;
                match lit {
                    Lit::Str(s) => params
                        .rpc_port
                        .replace(s.value().parse().expect("correct value")),
                    Lit::Int(i) => params
                        .rpc_port
                        .replace(i.base10_parse().expect("correct value")),
                    x => panic!("invalid attribute value '{:#?}'", x),
                };
            }
            "net" => {
                let lit = attr.value.lit;
                match lit {
                    Lit::Str(s) => match s.value().to_lowercase().as_str() {
                        "testnet" => params.net.replace("testnet".to_string()),
                        "mainnet" => params.net.replace("mainnet".to_string()),
                        x => panic!("invalid attribute value '{}'", x),
                    },
                    x => panic!("invalid attribute value '{:#?}'", x),
                };
            }
            x => panic!("unexpected attribute '{}'", x),
        }
    }

    // Validate and extract arguments
    let args = validate_extract_args(f.args.iter().map(|x| x.clone()));
    // Expand into gWasm connector code
    // TODO this could potentially be unsafe (passing strings like this).
    // Perhaps this could be weeded out with a custom cargo-gaas tool.
    let fn_vis = f.vis;
    let fn_ident = f.ident;
    let fn_args = f.args;
    let fn_ret = f.ret;

    let mut subtasks = vec![];
    let args_pats: Vec<_> = args.iter().map(|(pat, _)| pat.clone()).collect();
    for pat in &args_pats {
        let ts = quote!(.push_subtask_data(Vec::from(#pat)));
        subtasks.push(ts);
    }
    let datadir = params.datadir.unwrap_or_else(|| {
        appdirs::user_data_dir(Some("golem"), Some("golem"), false)
            .expect("existing project app datadirs")
            .join("default")
            .to_str()
            .expect("valid Unicode path")
            .to_owned()
    });
    let rpc_address = params.rpc_address.unwrap_or("127.0.0.1".to_string());
    let rpc_port = params.rpc_port.unwrap_or(61000);
    let net = params.net.unwrap_or("testnet".to_string());
    // Compute out dir
    let out_dir = env::var("GFAAS_OUT_DIR").expect("GFAAS_OUT_DIR should be defined");
    let local_testing = env::var("GFAAS_LOCAL");
    let input_data = args_pats[0].clone();
    let output = if let Ok(_) = local_testing {
        quote! {
            #fn_vis async fn #fn_ident(#fn_args) #fn_ret {
                use gfaas::__private::sp_wasm_engine::prelude::*;
                use gfaas::__private::tokio::task;
                use gfaas::__private::tempfile::tempdir;
                use gfaas::__private::lazy_static::lazy_static;
                use std::fs;
                use std::mem::ManuallyDrop;
                use std::path::Path;
                use std::sync::Arc;

                lazy_static! {
                    static ref ENGINE: Arc<JSEngine> = JSEngine::init().unwrap();
                }

                let data = Vec::from(#input_data);
                let engine = Arc::clone(&ENGINE);

                task::spawn_blocking(move || {
                    let js = Path::new(#out_dir).join("bin").join(format!("{}.js", stringify!(#fn_ident)));
                    let wasm = Path::new(#out_dir).join("bin").join(format!("{}.wasm", stringify!(#fn_ident)));
                    let workspace = ManuallyDrop::new(tempdir().unwrap());
                    let input_dir = workspace.path().join("in");
                    let output_dir = workspace.path().join("out");
                    fs::create_dir(&input_dir).unwrap();
                    fs::create_dir(&output_dir).unwrap();
                    fs::write(input_dir.join("in"), data).unwrap();

                    Sandbox::new(engine)
                        .and_then(|sandbox| sandbox.set_exec_args(vec!["in", "out"]))
                        .and_then(|sandbox| sandbox.load_input_files(input_dir))
                        .and_then(|sandbox| sandbox.run(js, wasm))
                        .and_then(|sandbox| sandbox.save_output_files(&output_dir, vec!["out"]))
                        .unwrap();

                    fs::read(output_dir.join("out")).unwrap()
                }).await.unwrap()
            }
        }
    } else {
        quote! {
            #fn_vis async fn #fn_ident(#fn_args) #fn_ret {
                use gfaas::__private::gwasm_api::prelude::*;
                use gfaas::__private::gwasm_api::golem;
                use gfaas::__private::tempfile::tempdir;
                use std::fs;
                use std::path::Path;
                use std::io::Read;

                struct ProgressTracker;

                impl ProgressUpdate for ProgressTracker {
                    fn update(&self, _progress: f64) {}
                }

                let workspace = tempdir().expect("could create a temp directory");
                let js = fs::read(Path::new(#out_dir).join("bin").join(format!("{}.js", stringify!(#fn_ident)))).unwrap();
                let wasm = fs::read(Path::new(#out_dir).join("bin").join(format!("{}.wasm", stringify!(#fn_ident)))).unwrap();
                let binary = GWasmBinary {
                    js: &js,
                    wasm: &wasm,
                };
                let task = TaskBuilder::new(workspace.path(), binary)
                    #(#subtasks)*
                    .build()
                    .unwrap();
                let computed_task = golem::compute(
                    Path::new(#datadir),
                    #rpc_address,
                    #rpc_port,
                    task,
                    match #net {
                        "testnet" => Net::TestNet,
                        "mainnet" => Net::MainNet,
                        _ => unreachable!(),
                    },
                    ProgressTracker,
                    None,
                )
                .await
                .unwrap();

                let mut out = vec![];
                for subtask in computed_task.subtasks {
                    for (_, mut reader) in subtask.data {
                        reader.read_to_end(&mut out).unwrap();
                    }
                }
                out
            }
        }
    };

    // TODO here goes the actual contents of the Wasm module
    let mut inputs = vec![];
    let mut input_args = vec![];
    for i in 0..args.len() {
        let in_ident = format_ident!("in{}", i);
        let ts = quote! {
            let next_arg = args.pop().unwrap();
            let mut f = File::open(next_arg).unwrap();
            let mut #in_ident = Vec::new();
            f.read_to_end(&mut #in_ident).unwrap();
        };
        inputs.push(ts);
        input_args.push(quote!(&#in_ident));
    }
    let contents = quote! {
        #preserved

        fn main() {
            use std::fs::File;
            use std::io::{Read, Write};
            use std::env;

            let mut args: Vec<_> = env::args().collect();
            let out = args.pop().unwrap();
            #(#inputs)*

            let res = #fn_ident(#(#input_args),*);

            let mut f = File::create(out).unwrap();
            f.write_all(&res).unwrap();
        }
    };

    // push body of the function into a Wasm module
    let out_path = Path::new(&out_dir)
        .join("gfaas_modules")
        .join("src")
        .join("bin")
        .join(format!("{}.rs", fn_ident.to_string()));
    let mut out = File::create(out_path).unwrap_or_else(|_| {
        panic!(
            "generating Wasm src file {}",
            [&out_dir, "gfaas.rs"].join("/")
        )
    });
    writeln!(out, "{}", contents).unwrap();

    output
}
