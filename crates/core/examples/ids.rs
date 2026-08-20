//! Prints real ids at each width and demonstrates ULID / UUIDv7 import.
//!
//! `cargo run -p notespace-core --example ids`

use notespace_core::id::{
    payload_bits, reserved_bits, PublicId, ID_CHARS, MAX_CHARS, MIN_CHARS, TIMESTAMP_BITS,
};

fn main() {
    let ts: u64 = 1_735_689_600_000;
    let random: u128 = 0x9E37_79B9_7F4A_7C15_1234;

    println!("\nSame timestamp ({ts}), every supported width:\n");
    println!("  {:<5} {:<7} {:<8}  id", "chars", "payload", "random");
    println!("  {}", "-".repeat(62));
    for chars in [MIN_CHARS, 18, 20, 22, MAX_CHARS] {
        let id = PublicId::with_width(
            ts,
            random >> (80 - (payload_bits(chars) - TIMESTAMP_BITS)),
            chars,
        )
        .unwrap();
        println!(
            "  {:<5} {:<7} {:<8}  {}{}",
            chars,
            format!("{} b", payload_bits(chars)),
            format!("{} b", payload_bits(chars) - TIMESTAMP_BITS),
            id,
            if chars == ID_CHARS {
                "   <- default"
            } else {
                ""
            }
        );
    }

    let narrow = PublicId::new(ts, (random >> 48) as u32).unwrap();
    let widest = PublicId::with_width(ts, random, MAX_CHARS).unwrap();
    println!(
        "\n  The {}-char id is a literal prefix of the {}-char one: {}",
        ID_CHARS,
        MAX_CHARS,
        widest.as_str().starts_with(narrow.as_str())
    );
    println!(
        "  Reserved low bits at {} chars: {} (always zero, or parse rejects it)",
        MAX_CHARS,
        reserved_bits(MAX_CHARS)
    );

    println!("\n\nImporting external 128-bit ids:\n");
    for (label, imported) in [
        (
            "ULID   ",
            PublicId::from_ulid("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap(),
        ),
        (
            "UUIDv7 ",
            PublicId::from_uuid("01890a5d-ac96-774b-bcce-b302099a8057").unwrap(),
        ),
    ] {
        println!(
            "  {label} source text   {}",
            if label.starts_with("ULID") {
                "01ARZ3NDEKTSV4RRFFQ69G5FAV".to_string()
            } else {
                "01890a5d-ac96-774b-bcce-b302099a8057".to_string()
            }
        );
        println!("          stored as     {imported}   <- re-aligned, still one of ours");
        println!("          to_u128()     {:#034x}", imported.to_u128());
        println!("          timestamp_ms  {}", imported.timestamp_ms());
        println!("          back to ULID  {}", imported.to_ulid());
        println!("          back to UUID  {}\n", imported.to_uuid());
    }

    println!("  The value round-trips exactly; the text does not, because canonical ULID pads");
    println!("  at the top and this format reserves at the bottom. That is the trade that keeps");
    println!("  an imported id sorting and prefixing alongside natively generated ones.\n");
}
