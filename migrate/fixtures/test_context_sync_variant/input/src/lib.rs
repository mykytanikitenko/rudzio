use test_context::TestContext;

pub struct FileCtx {
    pub path: String,
}

impl TestContext for FileCtx {
    fn setup() -> Self {
        FileCtx {
            path: "/tmp/test.txt".to_owned(),
        }
    }

    fn teardown(self) {
        let _ = self.path;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_context::test_context;

    #[test_context(FileCtx)]
    #[test]
    fn reads_path(ctx: &mut FileCtx) {
        assert!(!ctx.path.is_empty());
    }
}
