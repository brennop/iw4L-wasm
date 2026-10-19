use super::capture_t5::f32_at_t5;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FlameTube {
    pub size: [f32; 2],
    pub length: f32,
    pub segments: f32,
    pub anim_loop_s: f32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct FlameStreamView {
    pub chunk_gravity: [f32; 2],
    pub chunk_max_size: f32,
    pub chunk_size: [f32; 2],
    pub chunk_size_rand: [f32; 2],
    pub chunk_dist_scalar: f32,
    pub chunk_speed: f32,
    pub chunk_decel: f32,
    pub chunk_life_s: f32,
    pub chunk_rot_rand: f32,
    pub fire_interval_s: [f32; 2],
    pub fire_first_life_frac: f32,
    pub fire_min_life_frac: f32,
    pub fire_min_life_age_s: [f32; 2],
    pub fuel: FlameTube,
    pub flame: FlameTube,
    pub fire_life_s: f32,
    pub fire_life_rand_s: f32,
    pub fire_speed_scale: f32,
    pub fire_speed_scale_rand: f32,
    pub fire_vel_add_z: f32,
    pub fire_vel_add_z_rand: f32,
    pub fire_vel_add_sideways: f32,
    pub fire_gravity: [f32; 2],
    pub fire_max_rot_vel: f32,
    pub fire_friction: f32,
    pub fire_end_size_add: f32,
    pub fire_start_size_scale: f32,
    pub fire_end_size_scale: f32,
    /// `FlameTable` material order: fire, smoke, heat, drips, fuel, fuel2,
    /// flame, flame2.
    pub materials: [Option<String>; fastfile_t5::FLAME_TABLE_MATERIAL_COUNT],
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct WeaponFlameStream {
    pub views: [FlameStreamView; 2],
}

impl WeaponFlameStream {
    pub fn view(&self, player_view: bool) -> &FlameStreamView {
        &self.views[usize::from(!player_view)]
    }
}

pub(super) fn capture_t5_flame_stream(
    stream: &fastfile_t5::ZoneStream<'_>,
    geometry: &fastfile_t5::WeaponGeometry,
) -> Option<Box<WeaponFlameStream>> {
    use fastfile_t5::size as sz;
    let body = geometry.weap_def?;
    let mut views: [FlameStreamView; 2] = Default::default();
    for (view, (off, names)) in views.iter_mut().zip(
        [
            sz::WEAPON_DEF_FLAME_TABLE_FIRST_PERSON_OFF,
            sz::WEAPON_DEF_FLAME_TABLE_THIRD_PERSON_OFF,
        ]
        .into_iter()
        .zip(&geometry.flame_material_names),
    ) {
        let table = match stream.ptr_at(body, off).ok()? {
            fastfile_t5::ZonePtr::Offset(q) => stream.resolve_alias(q),
            _ => return None,
        };
        let f = |i: usize| f32_at_t5(stream, table, i * 4);
        let tube = |i: usize| FlameTube {
            size: [f(i), f(i + 1)],
            length: f(i + 2),
            segments: f(i + 3),
            anim_loop_s: f(i + 4),
        };
        let name = |slot: usize| {
            names[slot]
                .and_then(|ptr| stream.cstr(ptr).ok())
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
        };
        *view = FlameStreamView {
            chunk_gravity: [f(0), f(1)],
            chunk_max_size: f(2),
            chunk_size: [f(3), f(4)],
            chunk_size_rand: [f(5), f(6)],
            chunk_dist_scalar: f(7),
            chunk_speed: f(10),
            chunk_decel: f(11),
            chunk_life_s: f(13),
            chunk_rot_rand: f(28),
            fire_interval_s: [f(18), f(19)],
            fire_first_life_frac: f(20),
            fire_min_life_frac: f(22),
            fire_min_life_age_s: [f(23), f(24)],
            fuel: tube(40),
            flame: tube(45),
            fire_life_s: f(58),
            fire_life_rand_s: f(59),
            fire_speed_scale: f(60),
            fire_speed_scale_rand: f(61),
            fire_vel_add_z: f(62),
            fire_vel_add_z_rand: f(63),
            fire_vel_add_sideways: f(64),
            fire_gravity: [f(65), f(66)],
            fire_max_rot_vel: f(67),
            fire_friction: f(68),
            fire_end_size_add: f(69),
            fire_start_size_scale: f(70),
            fire_end_size_scale: f(71),
            materials: std::array::from_fn(name),
        };
    }
    Some(Box::new(WeaponFlameStream { views }))
}
