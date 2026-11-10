//! Serve the API over the in-memory fake backend (for poking at it with the
//! SDKs): `cargo run -p api --example fake_server -- 127.0.0.1:3001`.
//! A block with sample events is produced every 500 ms; every /exchange
//! action gets a plausible receipt (orders rest, cancels succeed).
use api::testkit::*;
use interfaces::*;
use std::time::Duration;

#[tokio::main]
async fn main() {
    let addr = std::env::args().nth(1).unwrap_or_else(|| "127.0.0.1:3001".into());
    let backend = FakeBackend::new(test_genesis());
    let user = addr_of(1);
    {
        let mut st = backend.state.lock();
        st.accounts.insert(
            user,
            AccountView {
                margin_summary: MarginSummary { account_value: usd("10000"), ..Default::default() },
                cross_margin_summary: MarginSummary { account_value: usd("10000"), ..Default::default() },
                withdrawable: usd("10000"),
                spot_balances: vec![(0, usd("100")), (1, usd("50"))],
                ..Default::default()
            },
        );
        st.books.insert(0, (vec![(px("2999"), sz("1"), 1)], vec![(px("3001"), sz("1"), 1)]));
    }
    backend.set_responder(simple_responder(user));
    let b2 = backend.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            b2.push_block(vec![], vec![Event::BookChanged { asset: 0 }]);
        }
    });
    eprintln!("fake api on {addr}; funded user {}", user.to_hex());
    api::serve(backend, addr.parse().unwrap()).await.unwrap();
}

fn addr_of(n: u8) -> Address {
    addr(n)
}
