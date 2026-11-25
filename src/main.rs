//! liq-sim CLI — офлайн-інтерфейс до ядра симулятора (див. lib.rs).
//! Вхід: JSON-запит (файл аргументом або stdin), поле "op":
//!   ladder | simulate_pair | simulate | decode_config | decode_emode |
//!   decode_account
//! Вихід: JSON у stdout. Жодної мережі.

use alloy_primitives::U256;
use liq_sim::*;
use serde_json::{json, Value};
use std::io::Read;

fn u256_of(v: &Value) -> U256 {
    match v {
        Value::String(s) => s.parse::<U256>().unwrap_or(U256::ZERO),
        Value::Number(n) => U256::from(n.as_u64().unwrap_or(0)),
        _ => U256::ZERO,
    }
}

fn hex_bytes(s: &str) -> Vec<u8> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    let mut out = Vec::with_capacity(s.len() / 2);
    let b = s.as_bytes();
    let mut i = 0;
    while i + 2 <= b.len() {
        let hi = (b[i] as char).to_digit(16).unwrap_or(0) as u8;
        let lo = (b[i + 1] as char).to_digit(16).unwrap_or(0) as u8;
        out.push((hi << 4) | lo);
        i += 2;
    }
    out
}

fn words_of_hex(s: &str) -> Vec<U256> {
    hex_bytes(s)
        .chunks(32)
        .filter(|c| c.len() == 32)
        .map(U256::from_be_slice)
        .collect()
}

fn quote_of(v: &Value) -> Quote {
    match v.get("mode").and_then(|m| m.as_str()) {
        Some("fixed") => Quote::Fixed {
            amount_out: u256_of(&v["amount_out"]),
            venue: v["venue"].as_str().unwrap_or("UniswapV3").to_string(),
        },
        Some("ratio") => Quote::Ratio {
            num: v["num"].as_u64().unwrap_or(0),
            den: v["den"].as_u64().unwrap_or(1),
            venue: v["venue"].as_str().unwrap_or("UniswapV3").to_string(),
        },
        _ => Quote::Fail,
    }
}

fn bonus_of(v: &Value) -> BonusInputs {
    BonusInputs {
        reserve_cfg_word: u256_of(&v["reserve_cfg_word"]),
        premium_bps: v["premium_bps"].as_u64().unwrap_or(0),
        user_emode: v["user_emode"].as_u64().unwrap_or(0),
        reserve_id: v["reserve_id"].as_u64().map(|x| x as u32),
        emode_bitmap: if v["emode_bitmap"].is_null() {
            None
        } else {
            Some(u256_of(&v["emode_bitmap"]))
        },
        emode_category_words: if v["emode_category_words"].is_null() {
            None
        } else {
            Some(
                v["emode_category_words"]
                    .as_array()
                    .map(|a| a.iter().map(u256_of).collect())
                    .unwrap_or_default(),
            )
        },
    }
}

fn pair_input(req: &Value) -> SimPairInput {
    SimPairInput {
        user: req["user"].as_str().unwrap_or("").to_string(),
        debt_sym: req["debt_sym"].as_str().unwrap_or("").to_string(),
        coll_sym: req["coll_sym"].as_str().unwrap_or("").to_string(),
        dec_debt: req["dec_debt"].as_u64().unwrap_or(0) as u32,
        dec_coll: req["dec_coll"].as_u64().unwrap_or(0) as u32,
        debt_raw: u256_of(&req["debt_raw"]),
        coll_raw: u256_of(&req["coll_raw"]),
        hf_wad: u256_of(&req["hf_wad"]),
        block: req["block"].as_u64().unwrap_or(0),
        p_debt: req["p_debt"].as_f64().unwrap_or(0.0),
        p_coll: req["p_coll"].as_f64().unwrap_or(0.0),
        bonus: bonus_of(&req["bonus"]),
        gas_usd: req["gas_usd"].as_f64().unwrap_or(0.0),
        gas_price_gwei: req["gas_price_gwei"].as_f64().unwrap_or(0.0),
        pol_usd: req["pol_usd"].as_f64().unwrap_or(0.0),
        slippage_bps: req["slippage_bps"].as_f64().unwrap_or(DEFAULT_SLIPPAGE_BPS),
        max_only: req["max_only"].as_bool().unwrap_or(false),
        quote: quote_of(&req["quote"]),
    }
}

fn main() {
    let mut txt = String::new();
    let args: Vec<String> = std::env::args().collect();
    if let Some(path) = args.get(1) {
        txt = std::fs::read_to_string(path).expect("read request file");
    } else {
        std::io::stdin().read_to_string(&mut txt).expect("read stdin");
    }
    let req: Value = serde_json::from_str(&txt).expect("request json");
    let op = req["op"].as_str().unwrap_or("");
    let out: Value = match op {
        "ladder" => {
            let p = pair_input(&req);
            let inp = LadderInput {
                debt_sym: p.debt_sym,
                coll_sym: p.coll_sym,
                dec_debt: p.dec_debt,
                dec_coll: p.dec_coll,
                debt_raw: p.debt_raw,
                coll_raw: p.coll_raw,
                p_debt: p.p_debt,
                p_coll: p.p_coll,
                bonus_bps: req["bonus_bps"].as_u64().unwrap_or(0),
                premium_bps: req["premium_bps"].as_u64().unwrap_or(0),
                protocol_fee_bps: req["protocol_fee_bps"].as_u64().unwrap_or(0),
                gas_usd: p.gas_usd,
                slippage_bps: p.slippage_bps,
                max_only: p.max_only,
                quote: p.quote,
            };
            json!({ "best": serde_json::to_value(ladder_best(&inp)).unwrap() })
        }
        "simulate_pair" => simulate_pair(&pair_input(&req)),
        "simulate" => simulate(&pair_input(&req)),
        "decode_config" => {
            let raw = hex_bytes(req["reserve_data_hex"].as_str().unwrap_or(""));
            let word = reserve_cfg_word(&raw);
            json!({
                "bonusBps": liquidation_bonus_bps(word),
                "protocolFeeBps": liquidation_protocol_fee_bps(word),
            })
        }
        "decode_emode" => {
            let words = words_of_hex(req["category_data_hex"].as_str().unwrap_or(""));
            json!({ "bonusBps": emode_category_bonus_bps(&words) })
        }
        "decode_account" => {
            let raw = hex_bytes(req["account_data_hex"].as_str().unwrap_or(""));
            let a = decode_get_user_account_data(&raw);
            json!({
                "totalCollateralBase": a.total_collateral_base.to_string(),
                "totalDebtBase": a.total_debt_base.to_string(),
                "availableBorrowsBase": a.available_borrows_base.to_string(),
                "currentLiquidationThreshold": a.current_liquidation_threshold.to_string(),
                "ltv": a.ltv.to_string(),
                "healthFactor": a.health_factor.to_string(),
            })
        }
        _ => json!({ "error": format!("unknown op {op}") }),
    };
    println!("{}", serde_json::to_string(&out).unwrap());
}
