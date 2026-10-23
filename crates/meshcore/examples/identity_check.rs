//! Check that a MeshCore private key (64-byte hex, as printed by `get prv.key`) derives the
//! expected public key / node ID.
//!
//! `cargo run -p meshcore --example identity_check -- <prv-hex> [expected-pub-hex]`

use meshcore::identity::LocalIdentity;
use meshcore::util::{from_hex, to_hex};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(prv_hex) = args.first() else {
        eprintln!("usage: identity_check <prv-hex> [expected-pub-hex]");
        std::process::exit(2);
    };
    let mut prv = [0u8; 64];
    if !from_hex(&mut prv, prv_hex.trim()) {
        eprintln!("private key must be 128 hex chars");
        std::process::exit(2);
    }
    let id = LocalIdentity::from_private_key(&prv);
    let pub_hex = to_hex(id.pub_key());
    println!("public key: {pub_hex}");
    println!("valid for ECDH/login: {}", LocalIdentity::validate_private_key(&prv));
    if let Some(expected) = args.get(1) {
        let ok = pub_hex.eq_ignore_ascii_case(expected.trim());
        println!("matches expected: {ok}");
        std::process::exit(if ok { 0 } else { 1 });
    }
}
