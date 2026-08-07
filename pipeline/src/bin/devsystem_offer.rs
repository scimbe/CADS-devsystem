//! Real minimal "agent": signs a real `ct_common::channel::CapacityOffer` for one
//! devsystem stage role and POSTs it to a devsystem-web instance's real offer
//! intake endpoint (`web/src/main.rs`'s `submit_offer`). Deliberately just an HTTP
//! client with a real signing key -- no channel/network infrastructure of its own
//! -- so it can run on any host with network access to the target devsystem-web
//! instance, never co-located with it (#382 operator directive: every component
//! must be runnable on a different host, connected only by real channels/network
//! calls). This is the whole "minimum agent": the smallest thing that can
//! authentically bid for a role.
//!
//! Usage: devsystem_offer <api-base-url> <run-id> <stage-id> <price>
//!        [--units N] [--seed N | --key-file PATH]
//!
//! Identity: `--seed N` derives a deterministic key (repeatable demos/tests);
//! `--key-file PATH` loads a real persisted key if it exists, else generates one
//! with a real CSPRNG and saves it -- a real recurring agent keeps the same
//! identity across invocations. Defaults to `--key-file ./devsystem-agent.key`.

use ct_common::channel::{CapacityKind, CapacityOffer, ServiceType};
use ed25519_dalek::SigningKey;
use std::env;
use std::fs;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).expect("system clock before 1970").as_secs()
}

fn signing_key_from_file(path: &str) -> SigningKey {
    if let Ok(bytes) = fs::read(path) {
        if let Ok(arr) = <[u8; 32]>::try_from(bytes.as_slice()) {
            return SigningKey::from_bytes(&arr);
        }
        eprintln!("warning: {path} exists but is not a 32-byte key -- regenerating");
    }
    let mut csprng = rand_core::OsRng;
    let key = SigningKey::generate(&mut csprng);
    if let Err(e) = fs::write(path, key.to_bytes()) {
        eprintln!("warning: could not persist key to {path}: {e} -- this identity will not survive the next run");
    }
    key
}

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let (Some(api_base), Some(run_id), Some(stage_id), Some(price_str)) = (args.next(), args.next(), args.next(), args.next())
    else {
        eprintln!("usage: devsystem_offer <api-base-url> <run-id> <stage-id> <price> [--units N] [--seed N | --key-file PATH]");
        return ExitCode::FAILURE;
    };
    let Ok(price) = price_str.parse::<u64>() else {
        eprintln!("price must be a non-negative integer, got {price_str:?}");
        return ExitCode::FAILURE;
    };

    let mut units: u64 = 1;
    let mut seed: Option<u8> = None;
    let mut key_file: Option<String> = None;
    let rest: Vec<String> = args.collect();
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--units" => match rest.get(i + 1).and_then(|s| s.parse().ok()) {
                Some(u) => {
                    units = u;
                    i += 2;
                }
                None => {
                    eprintln!("--units needs a number");
                    return ExitCode::FAILURE;
                }
            },
            "--seed" => match rest.get(i + 1).and_then(|s| s.parse().ok()) {
                Some(s) => {
                    seed = Some(s);
                    i += 2;
                }
                None => {
                    eprintln!("--seed needs a number 0-255");
                    return ExitCode::FAILURE;
                }
            },
            "--key-file" => {
                key_file = rest.get(i + 1).cloned();
                i += 2;
            }
            other => {
                eprintln!("unknown argument: {other}");
                return ExitCode::FAILURE;
            }
        }
    }

    let signing_key = match seed {
        Some(s) => SigningKey::from_bytes(&[s; 32]),
        None => signing_key_from_file(&key_file.unwrap_or_else(|| "./devsystem-agent.key".to_string())),
    };
    let holder_hex: String = signing_key.verifying_key().to_bytes().iter().take(4).map(|b| format!("{b:02x}")).collect();

    let now = unix_now();
    let offer = CapacityOffer::sign_new_with_services(
        &signing_key,
        CapacityKind::CloudApiQuota,
        vec!["devsystem-agent".to_string()],
        units,
        price,
        "usd".to_string(),
        now,
        now + 300, // 5-minute floor -- a real recurring agent re-submits well before this
        vec![ServiceType::Custom(stage_id.clone())],
    );

    let url = format!("{}/api/runs/{}/offers/submit", api_base.trim_end_matches('/'), run_id);
    // #388: a still-gated endpoint 302s to the gate's login page, which itself
    // returns a real 200 -- reqwest's default redirect policy silently follows
    // that (POST downgraded to GET, same as a browser or `curl -L`) and this
    // tool would report a fabricated "accepted" success while the offer never
    // actually reached submit_offer. No redirects, ever: a still-gated deploy
    // now fails loudly with the real 3xx status instead of a false positive.
    let client = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_else(|_| reqwest::blocking::Client::new());
    match client.post(&url).json(&offer).send() {
        Ok(resp) => {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            if status.is_success() {
                println!("holder={holder_hex} stage={stage_id} price={price} units={units} -> accepted by {url}");
                ExitCode::SUCCESS
            } else {
                eprintln!("holder={holder_hex} stage={stage_id} -> rejected ({status}): {body}");
                ExitCode::FAILURE
            }
        }
        Err(e) => {
            eprintln!("could not reach {url}: {e}");
            ExitCode::FAILURE
        }
    }
}
