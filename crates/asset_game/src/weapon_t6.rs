const STAND_INS: &[(&str, &str)] = &[
    ("an94", "ak47_mp"),
    ("hk416", "m4_mp"),
    ("scar", "scar_mp"),
    ("sig556", "m16_mp"),
    ("saritch", "fal_mp"),
    ("sa58", "fal_mp"),
    ("type95", "famas_mp"),
    ("tar21", "tavor_mp"),
    ("xm8", "fn2000_mp"),
    ("mp7", "mp5k_mp"),
    ("vector", "kriss_mp"),
    ("qcw05", "uzi_mp"),
    ("pdw57", "p90_mp"),
    ("evoskorpion", "tmp_mp"),
    ("insas", "ump45_mp"),
    ("qbb95", "rpd_mp"),
    ("lsat", "mg4_mp"),
    ("mk48", "m240_mp"),
    ("hamr", "sa80_mp"),
    ("as50", "barrett_mp"),
    ("dsr50", "cheytac_mp"),
    ("svu", "m21_mp"),
    ("ballista", "cheytac_mp"),
    ("fnp45", "usp_mp"),
    ("fiveseven", "beretta_mp"),
    ("kard", "deserteagle_mp"),
    ("judge", "coltanaconda_mp"),
    ("beretta93r", "beretta393_mp"),
    ("saiga12", "aa12_mp"),
    ("870mcs", "spas12_mp"),
    ("srm1216", "striker_mp"),
    ("ksg", "m1014_mp"),
    ("smaw", "at4_mp"),
    ("usrpg", "rpg_mp"),
    ("fhj18", "stinger_mp"),
    ("riotshield", "riotshield_mp"),
    ("crossbow", "m79_mp"),
    ("knife_ballistic", "throwingknife_mp"),
    ("frag_grenade", "frag_grenade_mp"),
    ("sticky_grenade", "semtex_mp"),
    ("hatchet", "throwingknife_mp"),
    ("satchel_charge", "c4_mp"),
    ("claymore", "claymore_mp"),
    ("bouncingbetty", "claymore_mp"),
    ("flash_grenade", "flash_grenade_mp"),
    ("concussion_grenade", "concussion_grenade_mp"),
    ("willy_pete", "smoke_grenade_mp"),
    ("emp_grenade", "concussion_grenade_mp"),
    ("proximity_grenade", "concussion_grenade_mp"),
    ("sensor_grenade", "smoke_grenade_mp"),
    ("trophy_system", "claymore_mp"),
    ("tactical_insertion", "flare_mp"),
];

const TACTICAL_EQUIPMENT: &[&str] = &["tactical_insertion"];

pub fn is_tactical_equipment(name: &str) -> bool {
    TACTICAL_EQUIPMENT.contains(&base_name(name))
}

const SHEDS_STAND_IN_TRAIL: &[&str] = &["bouncingbetty", "trophy_system"];

pub fn sheds_stand_in_trail(name: &str) -> bool {
    SHEDS_STAND_IN_TRAIL.contains(&base_name(name))
}

const STAYS_PLANTED: &[&str] = &["sensor_grenade"];

pub fn stays_planted(name: &str) -> bool {
    STAYS_PLANTED.contains(&base_name(name))
}

const PLANTED_MODELS: &[(&str, &str)] = &[("tactical_insertion", "t6_wpn_tac_insert_world")];

pub fn planted_model(name: &str) -> Option<&'static str> {
    let base = base_name(name);
    PLANTED_MODELS
        .iter()
        .find(|(t6, _)| *t6 == base)
        .map(|(_, model)| *model)
}

pub const T6_EFFECTS: &[&str] = &[
    "misc/fx_equip_tac_insert_light_grn",
    "misc/fx_equip_tac_insert_light_red",
    "misc/fx_equip_tac_insert_exp",
    "weapon/bouncing_betty/fx_betty_explosion",
    "weapon/bouncing_betty/fx_betty_destroyed",
    "weapon/bouncing_betty/fx_betty_launch_dust",
    "weapon/bouncing_betty/fx_betty_light_green",
    "weapon/bouncing_betty/fx_betty_light_red",
    "weapon/trophy_system/fx_trophy_flash_lng",
    "weapon/trophy_system/fx_trophy_radius_detonation",
    "weapon/trophy_system/fx_trophy_light_friendly",
    "weapon/trophy_system/fx_trophy_light_enemy",
    "weapon/trophy_system/fx_trophy_deploy_impact",
    "weapon/grenade/fx_prox_grenade_scan_grn",
    "weapon/grenade/fx_prox_grenade_scan_red",
    "weapon/grenade/fx_prox_grenade_wrn_grn",
    "weapon/grenade/fx_prox_grenade_wrn_red",
    "weapon/grenade/fx_prox_grenade_impact_player_spwner",
    "weapon/grenade/fx_spark_disabled_weapon",
    "weapon/c4/fx_c4_light_green",
    "weapon/c4/fx_c4_light_red",
    "weapon/emp/fx_emp_explosion_equip",
    "explosions/fx_exp_equipment",
    "explosions/fx_exp_equipment_lg",
    "explosions/fx_flashbang",
    "weapon/grenade/fx_prox_grenade_exp",
    "weapon/satchel/fx_explosion_satchel_generic",
    "weapon/sensor_grenade/fx_sensor_exp_scan_friendly",
    "weapon/sensor_grenade/fx_sensor_exp_scan_enemy",
];

pub const T6_EQUIPMENT_SOUNDS: &[&str] = &[
    "wpn_claymore_alert",
    "fly_betty_jump",
    "fly_betty_explo",
    "dst_equipment_destroy",
    "dst_disable_spark",
    "dst_tac_insert_break",
    "wpn_taser_mine_zap",
    "wpn_taser_mine_tacmask",
    "wpn_trophy_alert",
    "wpn_trophy_spin",
    "fly_sensor_nade_lp",
];

pub const MELEE_WEAPON: &str = "knife_mp";

pub fn base_name(name: &str) -> &str {
    let mut base = name.strip_suffix("_mp").unwrap_or(name);
    for prefix in ["sf_", "dualoptic_", "gl_"] {
        if let Some(rest) = base.strip_prefix(prefix) {
            base = rest;
        }
    }
    for suffix in ["_dw", "_lh"] {
        if let Some(rest) = base.strip_suffix(suffix) {
            base = rest;
        }
    }
    base
}

pub fn stand_in_for(name: &str) -> Option<&'static str> {
    let base = base_name(name);
    STAND_INS
        .iter()
        .find(|(t6, _)| *t6 == base)
        .map(|(_, iw4)| *iw4)
}

pub fn capture_t6_string_table(
    load: &fastfile_t6::ZoneLoad,
    asset: &fastfile_t6::LoadedAsset,
) -> Option<crate::CapturedStringTable> {
    if asset.ty != fastfile_t6::AssetType::StringTable {
        return None;
    }
    let h = &asset.header;
    let word = |at: usize| Some(u32::from_le_bytes(h.get(at..at + 4)?.try_into().ok()?));
    let ptr = |raw: u32| {
        (raw != 0 && raw < 0xFFFF_FFFE).then(|| fastfile_t6::Ptr {
            block: ((raw - 1) >> 29) as u8,
            offset: (raw - 1) & 0x1FFF_FFFF,
        })
    };
    let text = |p: Option<fastfile_t6::Ptr>| {
        p.and_then(|p| load.blocks.cstr(p).ok())
            .map(|b| String::from_utf8_lossy(b).into_owned())
    };
    let name = text(ptr(word(0)?))?;
    let columns = usize::try_from(word(4)? as i32).ok()?;
    let rows = usize::try_from(word(8)? as i32).ok()?;
    let values = ptr(word(12)?);
    let mut cells = Vec::with_capacity(rows * columns);
    for i in 0..rows * columns {
        let cell = values.map(|v| v.at(8 * i as u32));
        let string = cell
            .and_then(|c| load.blocks.ptr_at(c).ok().flatten())
            .and_then(|p| load.blocks.cstr(p).ok())
            .map_or_else(String::new, |b| String::from_utf8_lossy(b).into_owned());
        cells.push(string);
    }
    Some(crate::CapturedStringTable {
        name,
        columns,
        rows,
        cells,
    })
}
