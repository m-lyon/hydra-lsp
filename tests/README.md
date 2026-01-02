# Integration Tests

This directory contains integration tests for the Hydra LSP server.

## Running Tests

```bash
# Run all tests
cargo test

# Run a specific test file
cargo test --test hover

# Run with output visible
cargo test -- --nocapture

# Update all snapshots (first run)
INSTA_UPDATE=always cargo test

# Review snapshots interactively (requires cargo-insta)
cargo install cargo-insta
cargo test
cargo insta review
```

## Snapshot Testing

This project uses [insta](https://insta.rs/) for snapshot testing. Snapshots are stored in the `snapshots/` directory next to each test file.

When a test fails:

1. Run `cargo test` to generate `.snap.new` files
2. Run `cargo insta review` to review changes interactively
3. Accept or reject each snapshot change

## Test Pattern

Tests follow this pattern:

1. Create a `TestContext` with a workspace fixture
2. Initialize the LSP server
3. Open a document or send LSP requests
4. Assert the response using `insta` snapshots

Example:

```rust
#[tokio::test]
async fn test_hover_on_target() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;
    
    let content = std::fs::read_to_string(ctx.workspace.path().join("config.yaml")).unwrap();
    ctx.open_document("config.yaml", content).await;
    
    let res = ctx.request::<request::HoverRequest>(params).await;
    
    insta::assert_snapshot!("hover_result", format!("{:?}", res));
}
```

## Adding New Tests

1. Add test workspace fixtures in `workspace/[name]/`
2. Create a new test file or add to existing one
3. Run test to create initial snapshot
4. Review and accept the snapshot
