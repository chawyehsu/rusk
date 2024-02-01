all: circuits keys wasm abi allcircuits state contracts rusk ## Build everything

help: ## Display this help screen
	@grep -h \
		-E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "\033[36m%-30s\033[0m %s\n", $$1, $$2}'

abi: ## Build the ABI
	$(MAKE) -C ./rusk-abi all

circuits: ## Compress and store all circuits
	$(MAKE) -C ./circuits $@

keys: circuits ## Create the keys for the circuits
	$(MAKE) -C ./rusk recovery-keys

state: keys wasm ## Create the network state
	$(MAKE) -C ./rusk recovery-state

wasm: setup-compiler ## Generate the WASM for all the contracts
	$(MAKE) -C ./contracts $@
	$(MAKE) -C ./rusk-abi $@

allcircuits: ## Build circuit crates
	$(MAKE) -j -C ./circuits all

contracts: ## Execute the test for all contracts
	$(MAKE) -j1 -C ./contracts all

test: keys wasm ## Run the tests
	$(MAKE) -C ./rusk-abi/ $@
	$(MAKE) -j -C ./circuits $@
	$(MAKE) state
	$(MAKE) -j1 -C ./contracts $@
	$(MAKE) -C ./rusk-recovery $@
	$(MAKE) -C ./rusk-prover/ $@
	$(MAKE) -C ./node-data $@
	$(MAKE) -C ./consensus $@
	$(MAKE) -C ./node $@
	$(MAKE) -C ./rusk/ $@
			
clippy: ## Run clippy
	$(MAKE) -j -C ./circuits $@
	$(MAKE) -j1 -C ./contracts $@
	$(MAKE) -C ./rusk-abi $@
	$(MAKE) -C ./rusk-profile $@
	$(MAKE) -C ./rusk-recovery $@
	$(MAKE) -C ./rusk-prover/ $@
	$(MAKE) -C ./node-data $@
	$(MAKE) -C ./consensus $@
	$(MAKE) -C ./node $@
	$(MAKE) -C ./rusk/ $@

run: keys state ## Run the server
	cargo run --release --bin rusk

rusk: keys state ## Build rusk binary
	$(MAKE) -C ./rusk build

COMPILER_VERSION=v0.2.0
setup-compiler: ## Setup the Dusk Contract Compiler
	@./scripts/setup-compiler.sh $(COMPILER_VERSION)

.PHONY: all abi circuits keys state wasm allcircuits contracts test run help rusk setup-compiler
