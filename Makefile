.PHONY: build-vscode

build-vscode:
	cargo build --release
	mkdir -p ../hydra-lsp-vscode/bundled/libs/bin
	cp ./target/release/hydrust ../hydra-lsp-vscode/bundled/libs/bin/
	@echo "✓ Built and copied to VS Code extension"