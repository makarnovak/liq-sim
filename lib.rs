//! liq-sim — офлайн-ядро симулятора/plan-builder ліквідацій Aave V3.
//!
//! Вірний порт Python:
//!   the internal Python reference implementation (simulator.py)
//!     (_ladder_best, simulate_pair, simulate, _bonus_and_premium,
//!      _emode_bonus_bps)
//!   the internal Python reference implementation (common.py)
//!     (liquidation_bonus_bps, liquidation_protocol_fee_bps,
//!      emode_category_bonus_bps, decode_getUserAccountData, константи)
//!
//! Математика — БЕЗ ЖОДНИХ ЗМІН: той самий порядок f64-операцій, ті самі
//! цілочисельні урізання (Python int() == trunc до нуля; значення тут
//! невід'ємні), та сама драбина фракцій і кламп за балансом застави.
//! Мережі немає: ціни оракула, котування unwind, газ і слова конфігів —
//! входи. Квoter у Python бере найкращий amountOut серед веню; тут
//! котування задається входом Quote (записане значення або детермінована
//! формула для паритет-фікстур).

use alloy_primitives::U256;
use serde::Serialize;
use std::collections::HashSet;

// ------------------------------------------------------------- константи
// Дзеркало common.py (пакети 2026-10-24/27).
pub const LADDER_FRACTIONS: [f64; 4] = [0.05, 0.125, 0.25, 0.50];
pub const CLOSE_FACTOR_BPS: u64 = 5000;
pub const REVERT_BUFFER_USD: f64 = 0.05;
pub const LIQ_GAS_UNITS: u64 = 1_043_975;
pub const DEFAULT_SLIPPAGE_BPS: f64 = 14.0;
pub const WAD: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);

// ------------------------------------------------------------- хелпери
/// Python float(int): коректно округлене (round-half-even) перетворення
/// U256 -> f64 на весь діапазон, включно з HF-сентинелом 2^256-1
/// (на відміну від виконавця, де U256::MAX -> +inf — там це не впливає
/// на гроші: HF-сентинел відсікається гейтом hf>=1 до будь-якої ціни).
pub fn u256_to_f64(v: U256) -> f64 {
    if v == U256::ZERO {
        return 0.0;
    }
    let bits = 256 - v.leading_zeros() as usize;
    if bits <= 53 {
        return v.to::<u64>() as f64;
    }
    let shift = bits - 53;
    let mut mant = (v >> shift).to::<u64>(); // старші 53 біти
    let dropped = v & ((U256::ONE << shift) - U256::ONE);
    let half = U256::ONE << (shift - 1);
    if dropped > half || (dropped == half && mant % 2 == 1) {
        mant += 1;
        if mant == (1u64 << 53) {
            // переповнення мантиси: 2^53 * 2^shift == 2^52 * 2^(shift+1)
            return ((mant >> 1) as f64) * 2f64.powi(shift as i32 + 1);
        }
    }
    (mant as f64) * 2f64.powi(shift as i32)
}

/// Python int(float) для невід'ємних значень: trunc до нуля.
/// Діапазон контуру « 2^128 (saturate на u128::MAX — поза діапазоном
/// ніхто не оперує; Python дав би точне велике ціле).
fn f64_trunc_to_u256(x: f64) -> U256 {
    if !(x > 0.0) {
        return U256::ZERO;
    }
    U256::from(x as u128)
}

/// serde_json без arbitrary_precision не вміє i128 — віддаємо рядком,
/// як усі великі сирі суми (U256 і так серіалізується hex-рядком).
fn ser_i128_as_string<S: serde::Serializer>(v: &i128, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&v.to_string())
}

/// Коректно округлене (round-half-even, одне округлення) ділення
/// цілих a/b -> f64: дзеркало Python `int / int`, де частка рахується
/// з точних цілих, а НЕ як float(a)/float(b) (подвійне округлення дає
/// розбіжність в 1 ulp на сумах > 2^53 — спіймано паритетом, кейс E).
/// Точний шлях — для la-lb <= 54 (усі грошові шляхи контуру); поза
/// діапазоном (лише дисплейний HF-сентинел) — звичайне ділення f64.
pub fn div_u256_f64(a: U256, b: U256) -> f64 {
    if a == U256::ZERO || b == U256::ZERO {
        return 0.0;
    }
    let la = 256 - a.leading_zeros() as i64;
    let lb = 256 - b.leading_zeros() as i64;
    if la - lb > 54 {
        return u256_to_f64(a) / u256_to_f64(b);
    }
    let t = 54 - (la - lb); // >= 0 у точному шляху
    let num = a << (t as usize);
    let q = num / b;
    let r = num % b;
    let lq = 256 - q.leading_zeros() as i64;
    let drop = (lq - 53).max(0) as usize;
    let mut mant = (q >> drop).to::<u64>();
    let mut exp = drop as i64 - t; // результат = mant * 2^exp
    let round_up = if drop > 0 {
        let dropped_q = q & ((U256::ONE << drop) - U256::ONE);
        let half = U256::ONE << (drop - 1);
        match dropped_q.cmp(&half) {
            std::cmp::Ordering::Greater => true,
            std::cmp::Ordering::Less => false,
            std::cmp::Ordering::Equal => r != U256::ZERO || mant % 2 == 1,
        }
    } else {
        // частка q точна, дрібна частина — r/b
        let two_r = r << 1;
        two_r > b || (two_r == b && mant % 2 == 1)
    };
    if round_up {
        mant += 1;
        if mant == (1u64 << 53) {
            mant >>= 1;
            exp += 1;
        }
    }
    (mant as f64) * 2f64.powi(exp as i32)
}

/// Python: units = raw_int / 10**dec (int/int, одне округлення).
fn units_of(raw: U256, dec: u32) -> f64 {
    div_u256_f64(raw, U256::from(10u64).pow(U256::from(dec)))
}

fn frac_label(frac: f64) -> String {
    // Python: f"{frac * 100:g}%"
    match frac {
        0.05 => "5%".to_string(),
        0.125 => "12.5%".to_string(),
        0.25 => "25%".to_string(),
        _ => "50%".to_string(),
    }
}

// ------------------------------------------------------------- декодери
// Дзеркало common.py: конфіг резерву — ПЕРШЕ слово getReserveData.
pub fn reserve_cfg_word(reserve_data: &[u8]) -> U256 {
    if reserve_data.len() < 32 {
        return U256::ZERO;
    }
    U256::from_be_slice(&reserve_data[..32])
}

/// Bonus: біти 32-47 конфіга резерву (WETH=10500, WBTC=10850 ончейн).
/// Через лімби — той самий результат, що Python (cfg >> 32) & 0xFFFF.
pub fn liquidation_bonus_bps(cfg_word: U256) -> u64 {
    (cfg_word.as_limbs()[0] >> 32) & 0xFFFF
}

/// liquidationProtocolFee: біти 152-167 конфіга резерву (типово 1000) —
/// лімб 2, зсув 152-128=24. НЕ 168-175 — там eMode category (гіпотезу
/// зсуву 168 спростовано ончейн-пробою 2026-10-27).
pub fn liquidation_protocol_fee_bps(cfg_word: U256) -> u64 {
    (cfg_word.as_limbs()[2] >> 24) & 0xFFFF
}

/// Бонус eMode-категорії з сирих слів getEModeCategoryData.
/// Розкладка пулу (struct через зсув): [0x20, ltv, LT, bonus, ...] ->
/// bonus — слово 3. Плоска розкладка: [ltv, LT, bonus, ...] -> слово 2.
pub fn emode_category_bonus_bps(words: &[U256]) -> u64 {
    let w = |i: usize| words.get(i).copied().unwrap_or(U256::ZERO);
    if words.len() >= 4 && w(0) == U256::from(0x20u64) {
        return w(3).to::<u64>();
    }
    w(2).to::<u64>()
}

#[derive(Debug, Clone, Serialize)]
pub struct AccountData {
    #[serde(rename = "totalCollateralBase")]
    pub total_collateral_base: U256,
    #[serde(rename = "totalDebtBase")]
    pub total_debt_base: U256,
    #[serde(rename = "availableBorrowsBase")]
    pub available_borrows_base: U256,
    #[serde(rename = "currentLiquidationThreshold")]
    pub current_liquidation_threshold: U256,
    pub ltv: U256,
    #[serde(rename = "healthFactor")]
    pub health_factor: U256,
}

/// Дзеркало common.decode_getUserAccountData (6 слів по 32 байти).
pub fn decode_get_user_account_data(raw: &[u8]) -> AccountData {
    let w = |i: usize| -> U256 {
        if raw.len() < (i + 1) * 32 {
            return U256::ZERO;
        }
        U256::from_be_slice(&raw[i * 32..(i + 1) * 32])
    };
    AccountData {
        total_collateral_base: w(0),
        total_debt_base: w(1),
        available_borrows_base: w(2),
        current_liquidation_threshold: w(3),
        ltv: w(4),
        health_factor: w(5),
    }
}

// --------------------------------------------------- бонус / премія / fee
/// Входи дзеркала simulator._bonus_and_premium + _emode_bonus_bps:
/// усе, що Python читає свіжими eth_call, тут подається значеннями.
#[derive(Debug, Clone)]
pub struct BonusInputs {
    /// Перше слово getReserveData(collateral) — конфіг резерву застави.
    pub reserve_cfg_word: U256,
    /// FLASHLOAN_PREMIUM_TOTAL() (bps).
    pub premium_bps: u64,
    /// getUserEMode(user): 0 — не в eMode.
    pub user_emode: u64,
    /// Reserve id застави (індекс у getAllReservesTokens) або None.
    pub reserve_id: Option<u32>,
    /// getEModeCategoryCollateralBitmap(cat) — треба лише при cat > 0.
    pub emode_bitmap: Option<U256>,
    /// Слова getEModeCategoryData(cat) — треба лише при біті в бітмапі.
    pub emode_category_words: Option<Vec<U256>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BonusResult {
    #[serde(rename = "bonusBps")]
    pub bonus_bps: u64,
    #[serde(rename = "premiumBps")]
    pub premium_bps: u64,
    #[serde(rename = "protocolFeeBps")]
    pub protocol_fee_bps: u64,
    #[serde(rename = "protocolFeeSource")]
    pub protocol_fee_source: String,
    #[serde(rename = "bonusSource")]
    pub bonus_source: String,
    #[serde(rename = "emodeCategory")]
    pub emode_category: u64,
    #[serde(rename = "reserveBonusBps")]
    pub reserve_bonus_bps: u64,
}

/// Дзеркало _bonus_and_premium: eMode-бонус чинний тільки якщо юзер у
/// категорії І біт застави стоїть у collateralBitmap категорії; інакше —
/// резервний бонус. ProtocolFee — завжди з конфіга резерву застави.
pub fn bonus_and_premium(inp: &BonusInputs) -> BonusResult {
    let reserve_bonus = liquidation_bonus_bps(inp.reserve_cfg_word);
    let fee_bps = liquidation_protocol_fee_bps(inp.reserve_cfg_word);
    let mut out = BonusResult {
        bonus_bps: reserve_bonus,
        premium_bps: inp.premium_bps,
        protocol_fee_bps: fee_bps,
        protocol_fee_source: "collateralReserveConfig".to_string(),
        bonus_source: "reserve".to_string(),
        emode_category: inp.user_emode,
        reserve_bonus_bps: reserve_bonus,
    };
    if inp.user_emode == 0 {
        out.emode_category = 0;
        return out;
    }
    let rid = match inp.reserve_id {
        Some(r) => r,
        None => return out, // Python: (None, cat_id) -> резервний бонус
    };
    let bitmap = inp.emode_bitmap.unwrap_or(U256::ZERO);
    if (bitmap >> rid) & U256::from(1u64) != U256::from(1u64) {
        return out; // застава не входить до категорії
    }
    let words = inp.emode_category_words.as_deref().unwrap_or(&[]);
    out.bonus_bps = emode_category_bonus_bps(words);
    out.bonus_source = "emode_category".to_string();
    out
}

// ------------------------------------------------------------- драбина
/// Котування unwind для одного щабля: Python бере найкращий amountOut
/// серед веню і кидає RuntimeError, якщо жоден quoter не відповів
/// (щабель пропускається). Тут котування — вхід.
#[derive(Debug, Clone)]
pub enum Quote {
    /// Жоден quoter не дав котування — щабель пропускається.
    Fail,
    /// Записане котування: (amountOut, venue).
    Fixed { amount_out: U256, venue: String },
    /// Детермінована формула фікстур: amountOut = amountIn * num / den.
    Ratio { num: u64, den: u64, venue: String },
}

impl Quote {
    fn eval(&self, amount_in: U256) -> Option<(U256, String)> {
        match self {
            Quote::Fail => None,
            Quote::Fixed { amount_out, venue } => Some((*amount_out, venue.clone())),
            Quote::Ratio { num, den, venue } => {
                if *den == 0 {
                    None
                } else {
                    Some((amount_in * U256::from(*num) / U256::from(*den), venue.clone()))
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct LadderInput {
    pub debt_sym: String,
    pub coll_sym: String,
    pub dec_debt: u32,
    pub dec_coll: u32,
    pub debt_raw: U256,
    pub coll_raw: U256,
    pub p_debt: f64,
    pub p_coll: f64,
    pub bonus_bps: u64,
    pub premium_bps: u64,
    pub protocol_fee_bps: u64,
    pub gas_usd: f64,
    pub slippage_bps: f64,
    pub max_only: bool,
    pub quote: Quote,
}

#[derive(Debug, Clone, Serialize)]
pub struct LadderStepOut {
    pub label: String,
    pub fraction: Option<f64>,
    #[serde(rename = "debtToCoverRaw")]
    pub debt_to_cover_raw: U256,
    /// Сире (нерівне Python round(...,2)) значення — округлення є
    /// презентацією; паритет звіряє сирі числа й округлені окремо.
    #[serde(rename = "debtToCoverUsdRaw")]
    pub debt_to_cover_usd_raw: f64,
    #[serde(rename = "netUsdRaw")]
    pub net_usd_raw: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct LadderBestOut {
    #[serde(rename = "targetUsdRaw")]
    pub target_usd_raw: f64,
    #[serde(rename = "ladderLabel")]
    pub ladder_label: String,
    #[serde(rename = "ladderFraction")]
    pub ladder_fraction: Option<f64>,
    #[serde(rename = "debtToCoverRaw")]
    pub debt_to_cover_raw: U256,
    #[serde(rename = "debtToCoverUnits")]
    pub debt_to_cover_units: f64,
    #[serde(rename = "collateralOutRaw")]
    pub collateral_out_raw: U256,
    #[serde(rename = "collateralNetRaw")]
    pub collateral_net_raw: U256,
    #[serde(rename = "protocolFeeRaw")]
    pub protocol_fee_raw: U256,
    #[serde(rename = "proceedsRaw")]
    pub proceeds_raw: U256,
    #[serde(rename = "premiumRaw")]
    pub premium_raw: U256,
    #[serde(rename = "profitRaw", serialize_with = "ser_i128_as_string")]
    pub profit_raw: i128,
    #[serde(rename = "netUsd")]
    pub net_usd: f64,
    #[serde(rename = "unwindVenue")]
    pub unwind_venue: String,
    #[serde(rename = "clampedToCollBalance")]
    pub clamped_to_coll_balance: bool,
    #[serde(rename = "ladderSteps")]
    pub ladder_steps: Vec<LadderStepOut>,
}

/// Дзеркало simulator._ladder_best — формула у формулу.
pub fn ladder_best(inp: &LadderInput) -> Option<LadderBestOut> {
    // pow_debt для ділень НЕ використовується: Python ділить int/int з
    // одним округленням (див. units_of) — float-поділ дав би 1 ulp різниці.
    let pow_coll = 10f64.powi(inp.dec_coll as i32);
    // 50% боргу — стеля close factor
    let max_cover_raw = inp.debt_raw * U256::from(CLOSE_FACTOR_BPS) / U256::from(10000u64);

    let mut steps_in: Vec<(Option<f64>, U256, String)> = Vec::new();
    if inp.max_only {
        steps_in.push((None, max_cover_raw, "MAX".to_string()));
    } else {
        let mut seen: HashSet<U256> = HashSet::new();
        for frac in LADDER_FRACTIONS {
            let cover_f = f64_trunc_to_u256(u256_to_f64(inp.debt_raw) * frac);
            let cover = cover_f.min(max_cover_raw);
            if cover > U256::ZERO && seen.insert(cover) {
                steps_in.push((Some(frac), cover, frac_label(frac)));
            }
        }
    }

    let mut best: Option<LadderBestOut> = None;
    let mut ladder_steps: Vec<LadderStepOut> = Vec::new();
    for (frac, cover0, label) in steps_in {
        let mut debt_to_cover = cover0;
        if debt_to_cover == U256::ZERO {
            continue;
        }
        // collateralOut в одиницях застави, з бонусом і без нього
        let mut coll_base_f = (units_of(debt_to_cover, inp.dec_debt)) * (inp.p_debt / inp.p_coll);
        let mut coll_out_f = coll_base_f * (inp.bonus_bps as f64 / 10000.0);
        let mut coll_out_raw = f64_trunc_to_u256(coll_out_f * pow_coll);
        if coll_out_raw == U256::ZERO {
            continue;
        }
        let mut clamped = false;
        if inp.coll_raw > U256::ZERO && coll_out_raw > inp.coll_raw {
            // Кламп за балансом застави: покриття, яке баланс реально тягне.
            debt_to_cover = debt_to_cover * inp.coll_raw / coll_out_raw;
            if debt_to_cover == U256::ZERO {
                continue;
            }
            coll_base_f = (units_of(debt_to_cover, inp.dec_debt)) * (inp.p_debt / inp.p_coll);
            coll_out_f = coll_base_f * (inp.bonus_bps as f64 / 10000.0);
            coll_out_raw = f64_trunc_to_u256(coll_out_f * pow_coll).min(inp.coll_raw);
            if coll_out_raw == U256::ZERO {
                continue;
            }
            clamped = true;
        }
        let coll_base_raw = f64_trunc_to_u256(coll_base_f * pow_coll);
        // liquidationProtocolFee: частка БОНУСНОЇ частини застави,
        // віднімається з застави ДО unwind.
        let mut fee_raw = U256::ZERO;
        if inp.protocol_fee_bps > 0 && coll_out_raw > coll_base_raw {
            fee_raw = (coll_out_raw - coll_base_raw) * U256::from(inp.protocol_fee_bps)
                / U256::from(10000u64);
        }
        let coll_net_raw = coll_out_raw - fee_raw;
        if coll_net_raw == U256::ZERO {
            continue;
        }
        let (proceeds_raw, venue) = if inp.coll_sym == inp.debt_sym {
            // Same-asset: свопу нема, proceeds = отримана застава.
            (coll_net_raw, "SAME_ASSET".to_string())
        } else {
            match inp.quote.eval(coll_net_raw) {
                None => continue, // жоден quoter — щабель пропускається
                Some((amount_out, v)) => (
                    f64_trunc_to_u256(
                        u256_to_f64(amount_out) * (1.0 - inp.slippage_bps / 10000.0),
                    ),
                    v,
                ),
            }
        };
        let premium_raw = debt_to_cover * U256::from(inp.premium_bps) / U256::from(10000u64);
        // profit може бути від'ємним (Python int) — рахуємо в i128,
        // діапазон сирих сум контуру « 2^127.
        let profit_raw: i128 = proceeds_raw.to::<u128>() as i128
            - debt_to_cover.to::<u128>() as i128
            - premium_raw.to::<u128>() as i128;
        let profit_units = if profit_raw >= 0 {
            div_u256_f64(U256::from(profit_raw as u128), U256::from(10u64).pow(U256::from(inp.dec_debt)))
        } else {
            -div_u256_f64(U256::from((-profit_raw) as u128), U256::from(10u64).pow(U256::from(inp.dec_debt)))
        };
        let net_usd = profit_units * inp.p_debt - inp.gas_usd;
        ladder_steps.push(LadderStepOut {
            label: label.clone(),
            fraction: frac,
            debt_to_cover_raw: debt_to_cover,
            debt_to_cover_usd_raw: units_of(debt_to_cover, inp.dec_debt) * inp.p_debt,
            net_usd_raw: net_usd,
        });
        let cand = LadderBestOut {
            target_usd_raw: units_of(debt_to_cover, inp.dec_debt) * inp.p_debt,
            ladder_label: label,
            ladder_fraction: frac,
            debt_to_cover_raw: debt_to_cover,
            debt_to_cover_units: units_of(debt_to_cover, inp.dec_debt),
            collateral_out_raw: coll_out_raw,
            collateral_net_raw: coll_net_raw,
            protocol_fee_raw: fee_raw,
            proceeds_raw,
            premium_raw,
            profit_raw,
            net_usd,
            unwind_venue: venue,
            clamped_to_coll_balance: clamped,
            ladder_steps: Vec::new(),
        };
        if best.as_ref().map(|b| net_usd > b.net_usd).unwrap_or(true) {
            best = Some(cand);
        }
    }
    if let Some(mut b) = best {
        b.ladder_steps = ladder_steps;
        Some(b)
    } else {
        None
    }
}

// ------------------------------------------------- simulate_pair / simulate
#[derive(Debug, Clone)]
pub struct SimPairInput {
    pub user: String,
    pub debt_sym: String,
    pub coll_sym: String,
    pub dec_debt: u32,
    pub dec_coll: u32,
    pub debt_raw: U256,
    pub coll_raw: U256,
    /// healthFactor із getUserAccountData (WAD-шкала, сирий int).
    pub hf_wad: U256,
    pub block: u64,
    pub p_debt: f64,
    pub p_coll: f64,
    pub bonus: BonusInputs,
    pub gas_usd: f64,
    pub gas_price_gwei: f64,
    pub pol_usd: f64,
    pub slippage_bps: f64,
    pub max_only: bool,
    pub quote: Quote,
}

/// Дзеркало simulator.simulate_pair (без HF-гейта): повертає той самий
/// словник результату (мінус поле ts — час не є математикою).
pub fn simulate_pair(inp: &SimPairInput) -> serde_json::Value {
    let hf = div_u256_f64(inp.hf_wad, U256::from(10u64).pow(U256::from(18u64)));
    if inp.p_debt <= 0.0 || inp.p_coll <= 0.0 {
        return serde_json::json!({
            "ok": false, "reason": "оракул дав нульову ціну",
            "user": inp.user, "debtAsset": inp.debt_sym,
            "collateralAsset": inp.coll_sym, "block": inp.block,
        });
    }
    let b = bonus_and_premium(&inp.bonus);
    let ladder_inp = LadderInput {
        debt_sym: inp.debt_sym.clone(),
        coll_sym: inp.coll_sym.clone(),
        dec_debt: inp.dec_debt,
        dec_coll: inp.dec_coll,
        debt_raw: inp.debt_raw,
        coll_raw: inp.coll_raw,
        p_debt: inp.p_debt,
        p_coll: inp.p_coll,
        bonus_bps: b.bonus_bps,
        premium_bps: b.premium_bps,
        protocol_fee_bps: b.protocol_fee_bps,
        gas_usd: inp.gas_usd,
        slippage_bps: inp.slippage_bps,
        max_only: inp.max_only,
        quote: inp.quote.clone(),
    };
    match ladder_best(&ladder_inp) {
        None => serde_json::json!({
            "ok": false, "reason": "жодна щабель не дала unwind-котування",
            "user": inp.user, "debtAsset": inp.debt_sym,
            "collateralAsset": inp.coll_sym, "block": inp.block,
            "bonusBps": b.bonus_bps, "bonusSource": b.bonus_source,
            "emodeCategory": b.emode_category,
            "reserveBonusBps": b.reserve_bonus_bps,
            "protocolFeeBps": b.protocol_fee_bps,
            "protocolFeeSource": b.protocol_fee_source,
        }),
        Some(best) => {
            let mut v = serde_json::json!({
                "ok": true,
                "user": inp.user,
                "debtAsset": inp.debt_sym, "collateralAsset": inp.coll_sym,
                "healthFactor": hf,
                "block": inp.block,
                "pDebtUsd": inp.p_debt, "pCollUsd": inp.p_coll,
                "bonusBps": b.bonus_bps, "premiumBps": b.premium_bps,
                "protocolFeeBps": b.protocol_fee_bps,
                "protocolFeeSource": b.protocol_fee_source,
                "bonusSource": b.bonus_source,
                "emodeCategory": b.emode_category,
                "reserveBonusBps": b.reserve_bonus_bps,
                "gasUsd": inp.gas_usd, "gasPriceGwei": inp.gas_price_gwei,
                "polUsd": inp.pol_usd, "liqGasUnits": LIQ_GAS_UNITS,
                "gate_net_positive": best.net_usd > REVERT_BUFFER_USD,
                "reason": null,
            });
            let bo = serde_json::to_value(&best).unwrap();
            for (k, val) in bo.as_object().unwrap() {
                v[k] = val.clone();
            }
            v
        }
    }
}

/// Дзеркало simulator.simulate: HF-гейт + ворота net > REVERT_BUFFER_USD.
pub fn simulate(inp: &SimPairInput) -> serde_json::Value {
    if inp.hf_wad >= WAD {
        return serde_json::json!({
            "ok": false, "reason": "hf>=1 (вже не ліквідовна)",
            "user": inp.user, "healthFactor": inp.hf_wad.to_string(),
            "block": inp.block,
        });
    }
    let mut res = simulate_pair(inp);
    if !res["ok"].as_bool().unwrap_or(false) {
        return res;
    }
    let net = res["netUsd"].as_f64().unwrap_or(0.0);
    let ok = net > REVERT_BUFFER_USD;
    res["ok"] = ok.into();
    res["gate_net_positive"] = ok.into();
    res["reason"] = if ok {
        serde_json::Value::Null
    } else {
        format!(
            "netUsd={:.4} <= буфер revert ${:.2}",
            net, REVERT_BUFFER_USD
        )
        .into()
    };
    res
}
