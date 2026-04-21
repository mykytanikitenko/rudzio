use syn::parse::{Parse, ParseStream};
use syn::{Ident, Path, Token, bracketed, parenthesized};

#[derive(Debug)]
pub struct MainArgs {
    pub configs: Vec<RuntimeConfig>,
}

#[derive(Debug)]
pub struct RuntimeConfig {
    pub runtime: Path,
    pub suite: Path,
    pub test: Path,
}

impl RuntimeConfig {
    /// Drop the constructor segment (`::new`) and keep just the runtime type.
    pub fn runtime_type(&self) -> Path {
        let take_n = self.runtime.segments.len().saturating_sub(1);
        let segments = self.runtime.segments.iter().take(take_n).cloned().collect();
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

        let suite_kw: Ident = content.parse()?;
        if suite_kw != "suite" {
            return Err(syn::Error::new_spanned(
                suite_kw,
                "expected `suite` keyword",
            ));
        }
        let _: Token![=] = content.parse()?;
        let suite: Path = content.parse()?;
        ensure_bare_path(&suite, "suite")?;
        let _: Token![,] = content.parse()?;

        let test_kw: Ident = content.parse()?;
        if test_kw != "test" {
            return Err(syn::Error::new_spanned(test_kw, "expected `test` keyword"));
        }
        let _: Token![=] = content.parse()?;
        let test: Path = content.parse()?;
        ensure_bare_path(&test, "test")?;

        let _: Option<Token![,]> = content.parse()?;

        Ok(Self {
            runtime,
            suite,
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
