//! Balance helper: a small trained network that steers the balancer while she stands (it never replaces it). Each
//! 1/30 s it reads her state and what hit her, and sets the agent's BalanceAssist: leg / spine stiffness, step height
//! and reach, pelvis pitch, keeping upright, faster swings, landing timing. A zero network is the balancer as built.
//! Trained offline by skyrim-delirium's fit-test `train` example; the game runs this same code through the C API.

use delirium::collision::Collision;
use delirium::nm::{Agent, BalanceAssist};
use delirium::nmdbm::Assist;
use delirium::prelude::*;

pub const OBS: usize = 55;
pub const ACT: usize = 8;
pub const HIDDEN: usize = 64;
/// how often the network runs; its output holds in between
pub const PERIOD: f32 = 1.0 / 30.0;

/// what hit her last (the host says, nm_agent_helper_hit)
pub const HIT_NONE: usize = 0;
pub const HIT_PUSH: usize = 1;
pub const HIT_SHOT: usize = 2;
pub const HIT_BURN: usize = 3;
pub const HIT_SHOCK: usize = 4;
pub const HIT_CHILL: usize = 5;
pub const HITS: usize = 6;

const PELVIS: usize = 0;
const THIGH: [usize; 2] = [1, 4];
const FOOT: [usize; 2] = [3, 6];
const CHEST: usize = 9;
const HEAD: usize = 12;
const MAGIC: &[u8; 4] = b"NMH1";

pub struct Net {
    pub sizes: [usize; 4],
    /// observation normalisation (from the untrained balancer's own runs)
    pub mean: Vec<f32>,
    pub std: Vec<f32>,
    pub w: Vec<f32>,
}

impl Net {
    pub fn len(sizes: [usize; 4]) -> usize {
        (0..3).map(|l| sizes[l] * sizes[l + 1] + sizes[l + 1]).sum()
    }

    /// the balancer as built: every output 0
    pub fn zero() -> Net {
        let sizes = [OBS, HIDDEN, HIDDEN, ACT];
        Net { sizes, mean: vec![0.0; OBS], std: vec![1.0; OBS], w: vec![0.0; Net::len(sizes)] }
    }

    pub fn forward(&self, w: &[f32], x: &[f32]) -> [f32; ACT] {
        let mut a: Vec<f32> = x.iter().zip(&self.mean).zip(&self.std).map(|((x, m), s)| ((x - m) / s).clamp(-5.0, 5.0)).collect();
        let mut o = 0;
        for l in 0..3 {
            let (n, m) = (self.sizes[l], self.sizes[l + 1]);
            let (wl, b) = (&w[o..o + n * m], &w[o + n * m..o + n * m + m]);
            a = (0..m).map(|j| (b[j] + wl[j * n..j * n + n].iter().zip(&a).map(|(w, x)| w * x).sum::<f32>()).tanh()).collect();
            o += n * m + m;
        }
        std::array::from_fn(|k| a[k])
    }

    pub fn save(&self, path: &str) -> std::io::Result<()> {
        let mut b = MAGIC.to_vec();
        b.extend(self.sizes.iter().flat_map(|&s| (s as u32).to_le_bytes()));
        b.extend(self.mean.iter().chain(&self.std).chain(&self.w).flat_map(|x| x.to_le_bytes()));
        let tmp = format!("{path}.tmp");
        std::fs::write(&tmp, b)?;
        std::fs::rename(&tmp, path)
    }

    pub fn load(path: &str) -> Option<Net> {
        Net::parse(&std::fs::read(path).ok()?)
    }

    pub fn parse(b: &[u8]) -> Option<Net> {
        if b.len() < 20 || &b[..4] != MAGIC || (b.len() - 20) % 4 != 0 {
            return None;
        }
        let u = |i: usize| u32::from_le_bytes(b[4 + i * 4..8 + i * 4].try_into().unwrap()) as usize;
        let sizes = [u(0), u(1), u(2), u(3)];
        if sizes != [OBS, sizes[1], sizes[2], ACT] || sizes[1] == 0 || sizes[2] == 0 || sizes[1] > 4096 || sizes[2] > 4096 {
            return None;
        }
        let f: Vec<f32> = b[20..].chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect();
        if f.len() != 2 * OBS + Net::len(sizes) || !f.iter().all(|x| x.is_finite()) {
            return None;
        }
        Some(Net { sizes, mean: f[..OBS].to_vec(), std: f[OBS..2 * OBS].iter().map(|s| s.max(1e-3)).collect(), w: f[2 * OBS..].to_vec() })
    }
}

/// One agent's helper state.
#[derive(Clone, Copy, Debug)]
pub struct State {
    pub prev: [f32; ACT],
    pub since_hit: f32,
    pub hit: usize,
    clock: f32,
}

impl Default for State {
    fn default() -> State {
        State { prev: [0.0; ACT], since_hit: 99.0, hit: HIT_NONE, clock: PERIOD }
    }
}

impl State {
    pub fn hit(&mut self, kind: usize) {
        (self.hit, self.since_hit) = (kind.min(HITS - 1), 0.0);
    }
}

/// her heading: the way the thighs face, on the ground plane
fn heading(fr: &[Transform]) -> Quat {
    let f = Vec3::Z.cross(fr[THIGH[1]].translation - fr[THIGH[0]].translation).with_z(0.0).normalize_or(Vec3::X);
    Quat::from_rotation_z(-f.y.atan2(f.x))
}

pub fn observe(a: &Agent, world: &Collision, st: &State) -> [f32; OBS] {
    let d = &a.doll;
    let fr: Vec<Transform> = d.frames().collect();
    let inv = heading(&fr);
    let pelvis = fr[PELVIS].translation;
    let (gd, gn) = world.cast(pelvis, -Vec3::Z, 3.0).unwrap_or((pelvis.z, Vec3::Z));
    let feet = (fr[FOOT[0]].translation + fr[FOOT[1]].translation) * 0.5;
    let (com, comv) = d.com();
    let mut o = Vec::with_capacity(OBS);
    o.push(gd);
    o.extend((inv * gn).to_array());
    o.extend((inv * (fr[CHEST].translation - pelvis).normalize_or(Vec3::Z)).to_array());
    o.extend((inv * (fr[HEAD].translation - fr[CHEST].translation).normalize_or(Vec3::Z)).to_array());
    o.extend((inv * (com - feet)).to_array());
    o.extend((inv * comv).to_array());
    for k in FOOT {
        o.extend((inv * (fr[k].translation - pelvis)).to_array());
    }
    for k in FOOT {
        o.extend((inv * d.motion(k).0 * 0.5).to_array());
    }
    o.extend(FOOT.map(|k| (d.touch.get(k).copied().unwrap_or(0) != 0) as u8 as f32));
    let stepping = a.balance_steps().map_or(3, |(_, s)| (s as usize).min(2));
    o.extend((0..4).map(|k| (k == stepping) as u8 as f32));
    for k in [PELVIS, CHEST] {
        o.extend((inv * d.motion(k).1 * 0.3).to_array());
    }
    o.push((-st.since_hit / 0.4).exp());
    o.extend((0..HITS).map(|k| (k == st.hit) as u8 as f32));
    o.extend(st.prev);
    debug_assert_eq!(o.len(), OBS);
    let mut x = [0.0; OBS];
    for (x, v) in x.iter_mut().zip(o) {
        *x = if v.is_finite() { v } else { 0.0 };
    }
    x
}

/// the network's outputs (-1..1 each, 0 = as built) as balancer adjustments
pub fn assist(act: &[f32; ACT]) -> BalanceAssist {
    let a = act.map(|x| x.clamp(-1.0, 1.0));
    BalanceAssist {
        leg_stiffness: 2f32.powf(0.5 * a[0]),
        step_height: 2f32.powf(a[1]),
        step_clamp: 2f32.powf(0.5 * a[2]),
        spine_stiffness: 2f32.powf(0.5 * a[3]),
        dbm: Assist { pitch: 0.15 * a[4], upright: 2.0 * a[5].max(0.0), fast: a[6].max(0.0), land: 0.5 + 0.2 * a[7] },
    }
}

/// Before each agent step: while she balances, run the network every PERIOD and hold its assist; otherwise none.
/// `w` is the network's weights (the trainer passes perturbed ones).
pub fn tick(net: &Net, w: &[f32], a: &mut Agent, world: &Collision, st: &mut State, dt: f32) {
    st.since_hit = (st.since_hit + dt).min(99.0);
    if a.balance_steps().is_none() {
        a.assist = None;
        (st.prev, st.clock) = ([0.0; ACT], PERIOD);
        return;
    }
    st.clock += dt;
    if st.clock < PERIOD {
        return;
    }
    st.clock = (st.clock - PERIOD).min(PERIOD);
    let act = net.forward(w, &observe(a, world, st));
    a.assist = Some(assist(&act));
    st.prev = act;
}
