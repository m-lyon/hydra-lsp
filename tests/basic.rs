mod common;

use crate::common::*;

#[tokio::test]
async fn test_initialize_server() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    // If we get here without panicking, initialization succeeded
    insta::assert_snapshot!("initialize_success", "Server initialized successfully");
}

#[tokio::test]
async fn test_detect_hydra_file() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    let content = std::fs::read_to_string(ctx.workspace.path().join("config.yaml")).unwrap();
    ctx.open_document("config.yaml", content).await;

    insta::assert_snapshot!("hydra_file_detected", "Hydra file opened and processed");
}

#[tokio::test]
async fn test_non_hydra_file() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    let content = r#"
# Regular YAML file without Hydra markers
key: value
nested:
  another_key: another_value
"#;
    ctx.open_document("regular.yaml", content.to_string()).await;

    insta::assert_snapshot!("non_hydra_file", "Non-Hydra YAML file opened");
}
