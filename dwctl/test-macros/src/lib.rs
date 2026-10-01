//! Database tests backed by dwctl's migrated template, with SQLx fixture syntax.

use proc_macro::TokenStream;
use quote::quote;
use syn::{ItemFn, LitStr, Token, parse_macro_input, punctuated::Punctuated};

/// Like `sqlx::test`, for dwctl unit tests taking one `PgPool` argument.
/// Tests requiring empty or partial schemas must keep using `sqlx::test`.
#[proc_macro_attribute]
pub fn test(args: TokenStream, input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as ItemFn);
    match expand(args.into(), input) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.into_compile_error().into(),
    }
}

fn expand(args: proc_macro2::TokenStream, input: ItemFn) -> syn::Result<proc_macro2::TokenStream> {
    let mut fixtures = Vec::<LitStr>::new();
    let parser = syn::meta::parser(|meta| {
        if !meta.path.is_ident("fixtures") {
            return Err(meta.error("only fixtures are supported; use sqlx::test for migration tests"));
        }
        let content;
        syn::parenthesized!(content in meta.input);
        if content.peek(LitStr) {
            for fixture in content.parse_terminated(|input| input.parse::<LitStr>(), Token![,])? {
                let value = fixture.value();
                let suffix = if value.ends_with(".sql") { "" } else { ".sql" };
                let path = if value.contains('/') {
                    format!("{value}{suffix}")
                } else {
                    format!("fixtures/{value}{suffix}")
                };
                fixtures.push(LitStr::new(&path, fixture.span()));
            }
        } else {
            let path_name: syn::Ident = content.parse()?;
            if path_name != "path" {
                return Err(syn::Error::new_spanned(path_name, "expected path"));
            }
            content.parse::<Token![=]>()?;
            let path: LitStr = content.parse()?;
            content.parse::<Token![,]>()?;
            let scripts: syn::Ident = content.parse()?;
            if scripts != "scripts" {
                return Err(syn::Error::new_spanned(scripts, "expected scripts"));
            }
            let names;
            syn::parenthesized!(names in content);
            for fixture in Punctuated::<LitStr, Token![,]>::parse_terminated(&names)? {
                let name = fixture.value();
                let suffix = if name.ends_with(".sql") { "" } else { ".sql" };
                fixtures.push(LitStr::new(&format!("{}/{name}{suffix}", path.value()), fixture.span()));
            }
            if !content.is_empty() {
                content.parse::<Token![,]>()?;
            }
        }
        Ok(())
    });
    syn::parse::Parser::parse2(parser, args)?;
    if input.sig.asyncness.is_none() || input.sig.inputs.len() != 1 {
        return Err(syn::Error::new_spanned(&input.sig, "expected an async test taking one PgPool"));
    }
    let ItemFn { attrs, sig, block, .. } = input;
    let name = &sig.ident;
    let output = &sig.output;
    Ok(quote! {
        #(#attrs)*
        #[::core::prelude::v1::test]
        fn #name() #output {
            #sig #block
            ::sqlx::test_block_on(crate::test::template::run(
                concat!(module_path!(), "::", stringify!(#name)),
                &[#((#fixtures, include_str!(#fixtures))),*],
                |pool| Box::pin(#name(pool)),
            ))
        }
    })
}
