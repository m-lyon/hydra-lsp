.PHONY: build-vscode

build-vscode:
	cargo build --release
	cp ./target/release/hydra-lsp ../hydra-lsp-vscode/bundled/libs/bin/
	@echo "✓ Built and copied to VS Code extension"