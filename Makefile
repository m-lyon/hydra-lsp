.PHONY: build-vscode

build-vscode:
	cargo build --release
	cp ./target/release/hydrust ../hydrust-vscode/bundled/libs/bin/
	@echo "✓ Built and copied to VS Code extension"