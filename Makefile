.DEFAULT_GOAL := help

.PHONY: help check format format-check lint test coverage coverage-html audit outdated clean

help: ## Show available targets
	@awk 'BEGIN { FS = ":.*## " } \
		/^[[:alnum:]_-]+:.*## / { printf "\033[36m%-30s\033[0m %s\n", $$1, $$2 }' \
		$(MAKEFILE_LIST)

check: format-check lint test coverage ## Run formatting check, lint, tests, and coverage

format: ## Format code
	cargo fmt

format-check: ## Check formatting
	cargo fmt -- --check

lint: ## Run clippy with warnings denied
	cargo clippy --all-targets --all-features -- -D warnings

test: ## Run tests
	cargo test

coverage: ## Generate coverage summary
	cargo llvm-cov --summary-only

coverage-html: ## Generate HTML coverage report
	cargo llvm-cov --html

audit: ## Audit dependencies for vulnerabilities
	cargo audit

outdated: ## Report outdated dependencies
	cargo outdated

clean: ## Remove build artifacts
	cargo clean
