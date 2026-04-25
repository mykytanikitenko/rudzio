use test_context::AsyncTestContext;

pub struct DeepCtx {
    pub depth: u32,
}

impl AsyncTestContext for DeepCtx {
    async fn setup() -> Self {
        DeepCtx { depth: 0 }
    }

    async fn teardown(self) {
        let _ = self.depth;
    }
}
