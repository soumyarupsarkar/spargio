use proc_macro::TokenStream;
use quote::quote;
use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::{Expr, ExprLit, ItemFn, Lit, MetaNameValue, Token};

#[derive(Default)]
struct MainArgs {
    shards: Option<Expr>,
    backend: Option<BackendArg>,
}

#[derive(Clone, Copy)]
enum BackendArg {
    Queue,
    IoUring,
}

impl BackendArg {
    fn parse(value: &Expr) -> syn::Result<Self> {
        let Expr::Lit(ExprLit {
            lit: Lit::Str(lit), ..
        }) = value
        else {
            return Err(syn::Error::new(
                value.span(),
                "backend must be a string literal: \"queue\" or \"io_uring\"",
            ));
        };

        match lit.value().as_str() {
            "queue" => Ok(Self::Queue),
            "io_uring" => Ok(Self::IoUring),
            other => Err(syn::Error::new(
                lit.span(),
                format!("unsupported backend '{other}'; expected \"queue\" or \"io_uring\""),
            )),
        }
    }

    fn as_tokens(self) -> proc_macro2::TokenStream {
        match self {
            Self::Queue => quote!(::spargio::BackendKind::Queue),
            Self::IoUring => quote!(::spargio::BackendKind::IoUring),
        }
    }
}

impl MainArgs {
    fn parse(args: TokenStream) -> syn::Result<Self> {
        let mut out = Self::default();
        let parser = Punctuated::<MetaNameValue, Token![,]>::parse_terminated;
        let args = parser.parse(args)?;
        for arg in args {
            if arg.path.is_ident("shards") {
                if out.shards.is_some() {
                    return Err(syn::Error::new(arg.path.span(), "duplicate 'shards' option"));
                }
                out.shards = Some(arg.value);
                continue;
            }
            if arg.path.is_ident("backend") {
                if out.backend.is_some() {
                    return Err(syn::Error::new(
                        arg.path.span(),
                        "duplicate 'backend' option",
                    ));
                }
                out.backend = Some(BackendArg::parse(&arg.value)?);
                continue;
            }
            return Err(syn::Error::new(
                arg.path.span(),
                "unsupported option; expected one of: shards, backend",
            ));
        }
        Ok(out)
    }
}

#[proc_macro_attribute]
pub fn main(args: TokenStream, item: TokenStream) -> TokenStream {
    let args = match MainArgs::parse(args) {
        Ok(args) => args,
        Err(err) => return err.to_compile_error().into(),
    };

    let input = syn::parse_macro_input!(item as ItemFn);
    if input.sig.asyncness.is_none() {
        return syn::Error::new(
            input.sig.fn_token.span(),
            "#[spargio::main] can only be used on async functions",
        )
        .to_compile_error()
        .into();
    }
    if !input.sig.inputs.is_empty() {
        return syn::Error::new(
            input.sig.inputs.span(),
            "#[spargio::main] does not support function parameters",
        )
        .to_compile_error()
        .into();
    }
    if !input.sig.generics.params.is_empty() {
        return syn::Error::new(
            input.sig.generics.span(),
            "#[spargio::main] does not support generic parameters",
        )
        .to_compile_error()
        .into();
    }

    let attrs = input.attrs;
    let vis = input.vis;
    let name = input.sig.ident;
    let output = input.sig.output;
    let block = input.block;
    let inner_name = syn::Ident::new(&format!("__spargio_async_{}", name), name.span());

    let shards_builder = args
        .shards
        .map(|expr| quote!(.shards(#expr)))
        .unwrap_or_default();
    let backend_builder = args
        .backend
        .map(|backend| {
            let backend = backend.as_tokens();
            quote!(.backend(#backend))
        })
        .unwrap_or_default();

    quote!(
        #(#attrs)*
        #vis fn #name() #output {
            let __spargio_builder = ::spargio::Runtime::builder()
                #shards_builder
                #backend_builder;
            match ::spargio::run_with(__spargio_builder, |_spargio_handle| async move { #inner_name().await }) {
                Ok(__spargio_out) => __spargio_out,
                Err(::spargio::RuntimeError::UnsupportedBackend(__spargio_msg)) => {
                    panic!(
                        "spargio::main backend is not supported on this platform: {}",
                        __spargio_msg
                    )
                }
                Err(__spargio_err) => {
                    panic!("spargio::main runtime startup failed: {:?}", __spargio_err)
                }
            }
        }

        async fn #inner_name() #output {
            #block
        }
    )
    .into()
}
