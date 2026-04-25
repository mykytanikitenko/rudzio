use test_context::AsyncTestContext;

pub struct DbCtx {
    pub conn_id: u32,
}

impl AsyncTestContext for DbCtx {
    async fn setup() -> Self {
        DbCtx { conn_id: 42 }
    }

    async fn teardown(self) {
        let _ = self.conn_id;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_context::test_context;

    #[test_context(DbCtx)]
    #[tokio::test]
    async fn uses_db(ctx: &mut DbCtx) {
        assert_eq!(ctx.conn_id, 42);
    }
}
