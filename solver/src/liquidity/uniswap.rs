use anyhow::Result;
use contracts::{GPv2Settlement, UniswapV2Factory, UniswapV2Pair, UniswapV2Router02, IERC20};
use ethcontract::{batch::CallBatch, Http, Web3};
use hex_literal::hex;
use model::TokenPair;
use num::rational::Rational;
use primitive_types::{H160, U256};
use std::collections::{hash_map::Entry, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use web3::signing::keccak256;

const UNISWAP_PAIR_INIT_CODE: [u8; 32] =
    hex!("96e8ac4277198ff8b6f785478aa9a39f403cb768dd02cbee326c3e7da348845f");

const HONEYSWAP_PAIR_INIT_CODE: [u8; 32] =
    hex!("3f88503e8580ab941773b59034fb4b2a63e86dbc031b3633a925533ad3ed2b93");

const MAX_BATCH_SIZE: usize = 100;

use crate::interactions::UniswapInteraction;
use crate::settlement::Interaction;

use super::{AmmOrder, AmmSettlementHandling, LimitOrder};

pub struct UniswapLiquidity {
    inner: Arc<Inner>,
    web3: Web3<Http>,
    chain_id: u64,
    native_token_wrapper: H160,
}

struct Inner {
    factory: UniswapV2Factory,
    router: UniswapV2Router02,
    gpv2_settlement: GPv2Settlement,
    // Mapping of how much allowance the router has per token to spend on behalf of the settlement contract
    allowances: Mutex<HashMap<H160, U256>>,
}

impl UniswapLiquidity {
    pub fn new(
        factory: UniswapV2Factory,
        router: UniswapV2Router02,
        gpv2_settlement: GPv2Settlement,
        native_token_wrapper: H160,
        web3: Web3<Http>,
        chain_id: u64,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                factory,
                router,
                gpv2_settlement,
                allowances: Mutex::new(HashMap::new()),
            }),
            web3,
            chain_id,
            native_token_wrapper,
        }
    }

    /// Given a list of offchain orders returns the list of AMM liquidity to be considered
    pub async fn get_liquidity(
        &self,
        offchain_orders: impl Iterator<Item = &LimitOrder> + Send + Sync,
    ) -> Result<Vec<AmmOrder>> {
        let mut pools = HashMap::new();
        let mut tokens = HashSet::new();
        let mut batch = CallBatch::new(self.web3.transport());

        // Helper closure that enqueues the call to fetch the reserves for the token pair if it hasn't been fetched already
        let mut get_reserves_if_not_yet_tracked = |pair| {
            let vacant = match pools.entry(pair) {
                Entry::Occupied(_) => return,
                Entry::Vacant(vacant) => vacant,
            };
            let uniswap_pair_address =
                pair_address(&pair, self.inner.factory.address(), self.chain_id);
            let pair_contract = UniswapV2Pair::at(
                &self.inner.factory.raw_instance().web3(),
                uniswap_pair_address,
            );

            let future = pair_contract.get_reserves().batch_call(&mut batch);
            vacant.insert(future);
        };

        for order in offchain_orders {
            get_reserves_if_not_yet_tracked(
                TokenPair::new(order.buy_token, order.sell_token).expect("buy token = sell token"),
            );

            // Include every token with native token pair in the pools
            if let Some(pair) = TokenPair::new(order.buy_token, self.native_token_wrapper) {
                get_reserves_if_not_yet_tracked(pair);
            }
            if let Some(pair) = TokenPair::new(order.sell_token, self.native_token_wrapper) {
                get_reserves_if_not_yet_tracked(pair);
            }
        }
        batch.execute_all(MAX_BATCH_SIZE).await;

        let mut result = Vec::new();
        for (pair, future) in pools {
            if let Ok(reserves) = future.await {
                tokens.insert(pair.get().0);
                tokens.insert(pair.get().1);

                result.push(AmmOrder {
                    tokens: pair,
                    reserves: (reserves.0, reserves.1),
                    fee: Rational::new_raw(3, 1000),
                    settlement_handling: self.inner.clone(),
                })
            };
        }
        self.cache_allowances(tokens.into_iter()).await;
        Ok(result)
    }

    async fn cache_allowances(&self, tokens: impl Iterator<Item = H160>) {
        let mut batch = CallBatch::new(self.web3.transport());
        let results: Vec<_> = tokens
            .map(|token| {
                let allowance = IERC20::at(&self.web3, token)
                    .allowance(
                        self.inner.gpv2_settlement.address(),
                        self.inner.router.address(),
                    )
                    .view()
                    .batch_call(&mut batch);
                (token, allowance)
            })
            .collect();

        let _ = batch.execute_all(MAX_BATCH_SIZE).await;

        // await before acquiring lock so we can use std::sync::mutex (async::mutex wouldn't allow AmmSettlementHandling to be non-blocking)
        let mut token_and_allowance = Vec::with_capacity(results.len());
        for (pair, allowance) in results {
            token_and_allowance.push((pair, allowance.await.unwrap_or_default()));
        }

        self.inner
            .allowances
            .lock()
            .expect("Thread holding mutex panicked")
            .extend(token_and_allowance);
    }
}

impl Inner {
    fn _settle(&self, input: (H160, U256), output: (H160, U256)) -> UniswapInteraction {
        let set_allowance = self
            .allowances
            .lock()
            .expect("Thread holding mutex panicked")
            .get(&input.0)
            .cloned()
            .unwrap_or_default()
            < input.1;

        UniswapInteraction {
            contract: self.router.clone(),
            settlement: self.gpv2_settlement.clone(),
            set_allowance,
            amount_in: input.1,
            amount_out_min: output.1,
            token_in: input.0,
            token_out: output.0,
        }
    }
}

impl AmmSettlementHandling for Inner {
    fn settle(&self, input: (H160, U256), output: (H160, U256)) -> Vec<Box<dyn Interaction>> {
        vec![Box::new(self._settle(input, output))]
    }
}

fn pair_address(pair: &TokenPair, factory: H160, chain_id: u64) -> H160 {
    // https://uniswap.org/docs/v2/javascript-SDK/getting-pair-addresses/
    let mut packed = [0u8; 40];
    packed[0..20].copy_from_slice(pair.get().0.as_fixed_bytes());
    packed[20..40].copy_from_slice(pair.get().1.as_fixed_bytes());
    let salt = keccak256(&packed);
    let init_hash = match chain_id {
        100 => HONEYSWAP_PAIR_INIT_CODE,
        _ => UNISWAP_PAIR_INIT_CODE,
    };
    create2(factory, &salt, &init_hash)
}

fn create2(address: H160, salt: &[u8; 32], init_hash: &[u8; 32]) -> H160 {
    let mut preimage = [0xff; 85];
    preimage[1..21].copy_from_slice(address.as_fixed_bytes());
    preimage[21..53].copy_from_slice(salt);
    preimage[53..85].copy_from_slice(init_hash);
    H160::from_slice(&keccak256(&preimage)[12..])
}

#[cfg(test)]
mod tests {
    use crate::interactions::dummy_web3;

    use super::*;

    #[test]
    fn test_create2_mainnet() {
        // https://info.uniswap.org/pair/0x3e8468f66d30fc99f745481d4b383f89861702c6
        let mainnet_factory = H160::from_slice(&hex!("5C69bEe701ef814a2B6a3EDD4B1652CB9cc5aA6f"));
        let pair = TokenPair::new(
            H160::from_slice(&hex!("6810e776880c02933d47db1b9fc05908e5386b96")),
            H160::from_slice(&hex!("c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2")),
        )
        .unwrap();
        assert_eq!(
            pair_address(&pair, mainnet_factory, 1),
            H160::from_slice(&hex!("3e8468f66d30fc99f745481d4b383f89861702c6"))
        );
    }

    #[test]
    fn test_create2_xdai() {
        // https://info.honeyswap.org/pair/0x4505b262dc053998c10685dc5f9098af8ae5c8ad
        let mainnet_factory = H160::from_slice(&hex!("A818b4F111Ccac7AA31D0BCc0806d64F2E0737D7"));
        let pair = TokenPair::new(
            H160::from_slice(&hex!("71850b7e9ee3f13ab46d67167341e4bdc905eef9")),
            H160::from_slice(&hex!("e91d153e0b41518a2ce8dd3d7944fa863463a97d")),
        )
        .unwrap();
        assert_eq!(
            pair_address(&pair, mainnet_factory, 100),
            H160::from_slice(&hex!("4505b262dc053998c10685dc5f9098af8ae5c8ad"))
        );
    }

    impl Inner {
        fn new(allowances: HashMap<H160, U256>) -> Self {
            let web3 = dummy_web3::dummy_web3();
            Self {
                factory: UniswapV2Factory::at(&web3, H160::zero()),
                router: UniswapV2Router02::at(&web3, H160::zero()),
                gpv2_settlement: GPv2Settlement::at(&web3, H160::zero()),
                allowances: Mutex::new(allowances),
            }
        }
    }

    #[test]
    fn test_should_set_allowance() {
        let token_a = H160::from_low_u64_be(1);
        let token_b = H160::from_low_u64_be(2);
        let allowances = maplit::hashmap! {
            token_a => 100.into(),
            token_b => 200.into(),
        };

        let inner = Inner::new(allowances);

        // Token A below, equal, above
        let interaction = inner._settle((token_a, 50.into()), (token_b, 100.into()));
        assert_eq!(interaction.set_allowance, false);

        let interaction = inner._settle((token_a, 100.into()), (token_b, 100.into()));
        assert_eq!(interaction.set_allowance, false);

        let interaction = inner._settle((token_a, 150.into()), (token_b, 100.into()));
        assert_eq!(interaction.set_allowance, true);

        // Token B below, equal, above
        let interaction = inner._settle((token_b, 150.into()), (token_a, 100.into()));
        assert_eq!(interaction.set_allowance, false);

        let interaction = inner._settle((token_b, 200.into()), (token_a, 100.into()));
        assert_eq!(interaction.set_allowance, false);

        let interaction = inner._settle((token_b, 250.into()), (token_a, 100.into()));
        assert_eq!(interaction.set_allowance, true);

        // Untracked token
        let interaction =
            inner._settle((H160::from_low_u64_be(3), 1.into()), (token_a, 100.into()));
        assert_eq!(interaction.set_allowance, true);
    }
}