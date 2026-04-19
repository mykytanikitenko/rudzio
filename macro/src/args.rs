use syn::parse::{Parse, ParseStream};
use syn::{bracketed, parenthesized, Ident, Path, Token};

pub(crate) struct MainArgs {
    pub(crate) configs: Vec<RuntimeConfig>,
}

pub(crate) struct RuntimeConfig {
    pub(crate) runtime: Path,
    pub(crate) global: Path,
    pub(crate) test: Path,
}

impl RuntimeConfig {
    /// Drop the constructor segment (`::new`) and keep just the runtime type.
    pub(crate) fn runtime_type(&self) -> Path {
        let take_n = self.runtime.segments.len().saturating_sub(1);
        let segments = self
            .runtime
            .segments
            .iter()
            .take(take_n)
            .cloned()
            .collect();
        Path {
            leading_colon: self.runtime.leading_colon,
            segments,
        }
    }
}

impl Parse for RuntimeConfig {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let content;
        parenthesized!(content in input);

        let runtime_kw: Ident = content.parse()?;
        if runtime_kw != "runtime" {
            return Err(syn::Error::new_spanned(
                runtime_kw,
                "expected `runtime` keyword",
            ));
        }
        let _: Token![=] = content.parse()?;
        let runtime: Path = content.parse()?;
        let _: Token![,] = content.parse()?;

        let global_kw: Ident = content.parse()?;
        if global_kw != "global_context" {
            return Err(syn::Error::new_spanned(
                global_kw,
                "expected `global_context` keyword",
            ));
        }
        let _: Token![=] = content.parse()?;
        let global: Path = content.parse()?;
        ensure_bare_path(&global, "global_context")?;
        let _: Token![,] = content.parse()?;

        let test_kw: Ident = content.parse()?;
        if test_kw != "test_context" {
            return Err(syn::Error::new_spanned(
                test_kw,
                "expected `test_context` keyword",
            ));
        }
        let _: Token![=] = content.parse()?;
        let test: Path = content.parse()?;
        ensure_bare_path(&test, "test_context")?;

        let _: Option<Token![,]> = content.parse()?;

        Ok(Self {
            runtime,
            global,
            test,
        })
    }
}

fn ensure_bare_path(path: &Path, field: &str) -> syn::Result<()> {
    for seg in &path.segments {
        if !seg.arguments.is_empty() {
            return Err(syn::Error::new_spanned(
                seg,
                format!(
                    "`{field}` must be a bare type path; the macro injects the lifetime and runtime generics automatically"
                ),
            ));
        }
    }
    Ok(())
}

impl Parse for MainArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let content;
        bracketed!(content in input);

        let mut configs = Vec::new();
        while !content.is_empty() {
            configs.push(content.parse()?);
            if !content.is_empty() {
                let _: Token![,] = content.parse()?;
            }
        }

        if configs.is_empty() {
            return Err(syn::Error::new(
                content.span(),
                "expected at least one runtime configuration",
            ));
        }

        Ok(Self { configs })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quote::ToTokens;

    fn render(p: &impl ToTokens) -> String {
        p.to_token_stream().to_string().replace(' ', "")
    }

    #[::std::prelude::rust_2024::test]
    fn parses_new_keywords_global_context_and_test_context() {
        let config: RuntimeConfig = syn::parse_str(
            "( runtime = Multithread::new, global_context = common_context::Global, test_context = common_context::Test )"
        ).expect("new keyword form should parse");
        assert_eq!(render(&config.runtime), "Multithread::new");
        assert_eq!(render(&config.global), "common_context::Global");
        assert_eq!(render(&config.test), "common_context::Test");
    }

    #[::std::prelude::rust_2024::test]
    fn parser_accepts_trailing_comma_after_test_context() {
        let _: RuntimeConfig = syn::parse_str(
            "( runtime = Multithread::new, global_context = common_context::Global, test_context = common_context::Test, )"
        ).expect("trailing comma after test_context should be accepted");
    }

    fn parse_err_msg(source: &str) -> String {
        match syn::parse_str::<RuntimeConfig>(source) {
            Ok(_) => panic!("expected parse to fail for `{source}`"),
            Err(e) => e.to_string(),
        }
    }

    #[::std::prelude::rust_2024::test]
    fn rejects_legacy_global_keyword() {
        let msg = parse_err_msg(
            "( runtime = Multithread::new, global = common_context::Global, test_context = common_context::Test )"
        );
        assert!(
            msg.contains("global_context"),
            "error should mention the new `global_context` keyword, got: {msg}"
        );
    }

    #[::std::prelude::rust_2024::test]
    fn rejects_legacy_test_keyword() {
        let msg = parse_err_msg(
            "( runtime = Multithread::new, global_context = common_context::Global, test = common_context::Test )"
        );
        assert!(
            msg.contains("test_context"),
            "error should mention the new `test_context` keyword, got: {msg}"
        );
    }

    #[::std::prelude::rust_2024::test]
    fn rejects_turbofish_on_global_context() {
        let msg = parse_err_msg(
            "( runtime = Multithread::new, global_context = common_context::Global::<'_, Multithread>, test_context = common_context::Test )"
        );
        assert!(
            msg.contains("bare") || msg.contains("generic") || msg.contains("lifetime"),
            "error should explain that generics are injected by the macro, got: {msg}"
        );
    }

    #[::std::prelude::rust_2024::test]
    fn rejects_turbofish_on_test_context() {
        let msg = parse_err_msg(
            "( runtime = Multithread::new, global_context = common_context::Global, test_context = common_context::Test::<'_, Multithread> )"
        );
        assert!(
            msg.contains("bare") || msg.contains("generic") || msg.contains("lifetime"),
            "error should explain that generics are injected by the macro, got: {msg}"
        );
    }

    #[::std::prelude::rust_2024::test]
    fn runtime_type_strips_constructor_segment() {
        let config: RuntimeConfig = syn::parse_str(
            "( runtime = my_crate::nested::Multithread::new, global_context = common_context::Global, test_context = common_context::Test )"
        ).unwrap();
        assert_eq!(
            render(&config.runtime_type()),
            "my_crate::nested::Multithread"
        );
    }
}
