.PHONY: build test benchmark demo report http wasm clean

build:
	cargo build --release

test:
	cargo test

benchmark:
	./tools/benchmark.sh

demo:
	@echo "=== Verbose Demo ==="
	@echo ""
	@echo "--- Verify proofs ---"
	cargo run --quiet -- examples/showcase.verbose
	@echo ""
	@echo "--- Business report ---"
	cargo run --quiet -- examples/report.verbose --run total_revenue --input examples/report.json
	cargo run --quiet -- examples/report.verbose --run risk_score --input examples/report.json
	@echo ""
	@echo "--- JSON output ---"
	cargo run --quiet -- examples/report.verbose --run total_revenue --input examples/report.json --json
	@echo ""
	@echo "--- Native binary ---"
	cargo run --quiet -- examples/invoices.verbose --native /tmp/verbose_demo --run important_invoice
	/tmp/verbose_demo 15000 500 10001
	@rm -f /tmp/verbose_demo
	@echo ""
	@echo "--- WASM module ---"
	cargo run --quiet -- examples/invoices.verbose --wasm /tmp/verbose_demo.wasm --run important_invoice
	@rm -f /tmp/verbose_demo.wasm
	@echo ""
	@echo "--- Benchmark ---"
	cargo run --quiet -- examples/invoices.verbose --benchmark --run important_invoice

report:
	@for rule in total_revenue overdue_count overdue_amount risk_score; do \
		echo "--- $$rule ---"; \
		cargo run --quiet -- examples/report.verbose --run $$rule --input examples/report.json; \
		echo ""; \
	done

http:
	@echo "=== http target: tier-3 native emitter probe ==="
	@echo "Note: the server below is HARDCODED in native.rs (Rust), NOT described"
	@echo "      in any .verbose file. It proves the backend can emit tiny network"
	@echo "      binaries; network-primitives-in-.verbose is a future phase."
	@echo "      See docs/known-gaps.md 'Three tiers of native output'."
	@echo ""
	cargo run --quiet -- --demo-http /tmp/verbose-http
	@echo "Starting server... Press Ctrl+C to stop."
	/tmp/verbose-http

wasm:
	cargo run --quiet -- examples/business.verbose --wasm examples/demo.wasm --run total_with_tax
	@echo "WASM module: examples/demo.wasm"
	@echo "Open examples/demo.html in a browser (serve with: cd examples && python3 -m http.server 8000)"

clean:
	cargo clean
	rm -f /tmp/verbose-*
