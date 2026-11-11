#![allow(clippy::missing_safety_doc)]

pub mod character;
pub mod helper;

use delirium_data::taskparams::TaskParams;
use delirium::collision::Collision;
use delirium::msg::{self, Kind, Value};
use delirium::nm::{self, Agent};
use delirium::prelude::*;
use delirium::ragdoll::{Build, Obstacle, Ragdoll};
use std::cell::RefCell;
use std::ffi::{CStr, c_char};
use std::sync::RwLock;

pub const NM_E_ARG: i32 = -1;
pub const NM_E_IO: i32 = -2;
pub const NM_E_PARSE: i32 = -3;
pub const NM_E_MESSAGE: i32 = -4;
pub const NM_E_PARAM: i32 = -5;
pub const NM_E_TYPE: i32 = -6;
pub const NM_E_STATE: i32 = -7;
pub const NM_E_FOLLOW: i32 = -8;

pub const NM_MODE_LEAD: i32 = 0;
pub const NM_MODE_FOLLOW: i32 = 1;
/// Test hosts: the ragdoll alone (no muscles, no behaviours), moved only by what the host sets on it; stands in for a
/// game's own ragdoll when testing a FOLLOW agent offline.
pub const NM_MODE_BODY: i32 = 2;

thread_local! {
    static ERR: RefCell<String> = const { RefCell::new(String::new()) };
}
static TP: RwLock<Option<TaskParams>> = RwLock::new(None);
static RAND: std::sync::Mutex<delirium_data::rng::Rand> = std::sync::Mutex::new(delirium_data::rng::Rand { seed: 1 });
const SHOT_FORCE: f32 = 400.0;

fn fail(code: i32, why: impl Into<String>) -> i32 {
    ERR.with(|e| *e.borrow_mut() = why.into());
    code
}

unsafe fn text<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() { None } else { unsafe { CStr::from_ptr(p) }.to_str().ok() }
}

unsafe fn put(s: &str, buf: *mut c_char, len: i32) -> i32 {
    if !buf.is_null() && len > 0 {
        let n = s.len().min(len as usize - 1);
        unsafe {
            std::ptr::copy_nonoverlapping(s.as_ptr(), buf as *mut u8, n);
            *buf.add(n) = 0;
        }
    }
    s.len() as i32 + 1
}

unsafe fn v3(p: *const f32) -> Vec3 {
    unsafe { Vec3::new(*p, *p.add(1), *p.add(2)) }
}

unsafe fn put3(p: *mut f32, v: Vec3) {
    if !p.is_null() {
        unsafe { std::ptr::copy_nonoverlapping(v.to_array().as_ptr(), p, 3) }
    }
}

unsafe fn put4(p: *mut f32, q: Quat) {
    if !p.is_null() {
        unsafe { std::ptr::copy_nonoverlapping(q.to_array().as_ptr(), p, 4) }
    }
}

fn with_tp<R>(f: impl FnOnce(&TaskParams) -> R) -> R {
    let g = TP.read().unwrap_or_else(|e| e.into_inner());
    match g.as_ref() {
        Some(tp) => f(tp),
        None => f(&TaskParams::default()),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn nm_version() -> i32 {
    100
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_last_error(buf: *mut c_char, len: i32) -> i32 {
    ERR.with(|e| unsafe { put(&e.borrow(), buf, len) })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_init_memory(xrt: *const u8, len: i32) -> i32 {
    if xrt.is_null() || len <= 0 {
        return fail(NM_E_ARG, "nm_init_memory: no data");
    }
    let d = unsafe { std::slice::from_raw_parts(xrt, len as usize) };
    // Xbox 360 .xrt is big-endian, PC .art little-endian: try both, keep the one that parses to the full joint table
    let joints = |le: bool| {
        delirium_data::LE.store(le, std::sync::atomic::Ordering::Relaxed);
        std::panic::catch_unwind(|| delirium_data::nm_asset::read(d).len()).unwrap_or(0)
    };
    if joints(false) < 20 && joints(true) < 20 {
        delirium_data::LE.store(false, std::sync::atomic::Ordering::Relaxed);
        return fail(NM_E_PARSE, "nm_init: not an NM character file");
    }
    let fresh = delirium::nmrs::ASSET[delirium::nmrs::HOMER].get().is_none();
    delirium::nmrs::load(delirium::nmrs::HOMER, d);
    if fresh { 0 } else { 1 }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_init(xrt_path: *const c_char) -> i32 {
    let Some(p) = (unsafe { text(xrt_path) }) else { return fail(NM_E_ARG, "nm_init: no path") };
    match std::fs::read(p) {
        Ok(d) => unsafe { nm_init_memory(d.as_ptr(), d.len() as i32) },
        Err(e) => fail(NM_E_IO, format!("nm_init: {p}: {e}")),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_load_task_params(path: *const c_char) -> i32 {
    let Some(p) = (unsafe { text(path) }) else { return fail(NM_E_ARG, "nm_load_task_params: no path") };
    match std::fs::read_to_string(p) {
        Ok(t) => {
            let tp = delirium_data::taskparams::read(&t);
            *TP.write().unwrap_or_else(|e| e.into_inner()) = Some(tp);
            0
        }
        Err(e) => fail(NM_E_IO, format!("{p}: {e}")),
    }
}

/// Sets one task parameter (task, set or "*", name) on top of any loaded file, e.g. ("NMShot_Standing", "*", "shotMaxSteps", 10).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_set_task_param(task: *const c_char, set: *const c_char, name: *const c_char, value: f32) -> i32 {
    let (Some(t), Some(s), Some(n)) = (unsafe { text(task) }, unsafe { text(set) }, unsafe { text(name) }) else {
        return fail(NM_E_ARG, "nm_set_task_param: no task, set or name");
    };
    let mut g = TP.write().unwrap_or_else(|e| e.into_inner());
    g.get_or_insert_with(TaskParams::default).0.entry(t.to_string()).or_default().entry(s.to_string()).or_default().insert(n.to_string(), vec![value]);
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_part_name(i: i32, buf: *mut c_char, len: i32) -> i32 {
    match delirium_data::nm_asset::PARTS.get(i as usize) {
        Some(n) if i >= 0 => unsafe { put(n, buf, len) },
        _ => fail(NM_E_ARG, "nm_part_name: no such part"),
    }
}

pub struct NmWorld(Collision, Vec<Obstacle>);

#[unsafe(no_mangle)]
pub extern "C" fn nm_world_create() -> *mut NmWorld {
    Box::into_raw(Box::new(NmWorld(Collision::default(), vec![])))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_world_destroy(w: *mut NmWorld) {
    if !w.is_null() {
        drop(unsafe { Box::from_raw(w) });
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_world_add_mesh(w: *mut NmWorld, positions: *const f32, vertices: i32, triangles: *const u32, count: i32,
                                           materials: *const u8) -> i32 {
    let Some(w) = (unsafe { w.as_mut() }) else { return fail(NM_E_ARG, "null world") };
    if positions.is_null() || triangles.is_null() || vertices < 0 || count < 0 {
        return fail(NM_E_ARG, "nm_world_add_mesh: bad arrays");
    }
    let p = unsafe { std::slice::from_raw_parts(positions as *const [f32; 3], vertices as usize) };
    let t = unsafe { std::slice::from_raw_parts(triangles as *const [u32; 3], count as usize) };
    if t.iter().flatten().any(|&i| i as usize >= p.len()) {
        return fail(NM_E_ARG, "nm_world_add_mesh: index past the vertices");
    }
    let m = if materials.is_null() { vec![0; t.len()] } else { unsafe { std::slice::from_raw_parts(materials, t.len()) }.to_vec() };
    w.0.add(p, t, &m);
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_world_add_floor(w: *mut NmWorld, x: f32, y: f32, z: f32, half: f32) -> i32 {
    let Some(w) = (unsafe { w.as_mut() }) else { return fail(NM_E_ARG, "null world") };
    w.0.add_floor(x, y, z, half, 0);
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_world_set_boxes(w: *mut NmWorld, boxes: *const f32, count: i32) -> i32 {
    let Some(w) = (unsafe { w.as_mut() }) else { return fail(NM_E_ARG, "null world") };
    if count < 0 || (count > 0 && boxes.is_null()) {
        return fail(NM_E_ARG, "nm_world_set_boxes: bad array");
    }
    let f = if count == 0 { &[][..] } else { unsafe { std::slice::from_raw_parts(boxes, count as usize * 13) } };
    w.1 = f.chunks(13).enumerate().map(|(k, b)| {
        let c = Vec3::new(b[0], b[1], b[2]);
        let (rot, half, vel) = (Quat::from_xyzw(b[3], b[4], b[5], b[6]).normalize(), Vec3::new(b[7], b[8], b[9]), Vec3::new(b[10], b[11], b[12]));
        Obstacle { id: k as i32, at: c, rot, centre: c, half, vel, spin: Vec3::ZERO, com: c, parts: &[], last: None }
    }).collect();
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn nm_set_surface(material: i32, friction: f32, elasticity: f32) -> i32 {
    if !(0..256).contains(&material) || !(friction >= 0.0) || !(0.0..=1.0).contains(&elasticity) {
        return fail(NM_E_ARG, "nm_set_surface: material 0-255, friction >= 0, elasticity 0-1");
    }
    let mut s = delirium::ragdoll::SURFACES.write().unwrap();
    if s.len() <= material as usize {
        s.resize(material as usize + 1, [1.0, 0.1]);
    }
    s[material as usize] = [friction, elasticity];
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_world_triangle_count(w: *const NmWorld) -> i32 {
    unsafe { w.as_ref() }.map_or_else(|| fail(NM_E_ARG, "null world"), |w| w.0.len() as i32)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_world_raycast(w: *const NmWorld, from: *const f32, dir: *const f32, len: f32, dist: *mut f32, normal: *mut f32) -> i32 {
    let Some(w) = (unsafe { w.as_ref() }) else { return fail(NM_E_ARG, "null world") };
    if from.is_null() || dir.is_null() {
        return fail(NM_E_ARG, "null vector");
    }
    match w.0.cast(unsafe { v3(from) }, unsafe { v3(dir) }.normalize_or_zero(), len) {
        Some((t, n)) => {
            if !dist.is_null() {
                unsafe { *dist = t };
            }
            unsafe { put3(normal, n) };
            1
        }
        None => 0,
    }
}

pub struct NmBuild {
    build: Build,
    frames: Vec<Transform>,
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_build_from_text(t: *const c_char) -> *mut NmBuild {
    let Some(t) = (unsafe { text(t) }) else {
        fail(NM_E_ARG, "nm_build_from_text: no text");
        return std::ptr::null_mut();
    };
    let t = t.to_string();
    let r = std::panic::catch_unwind(move || Build::undump(&t));
    match r {
        Ok((build, frames)) if !frames.is_empty() => Box::into_raw(Box::new(NmBuild { build, frames })),
        _ => {
            fail(NM_E_PARSE, "nm_build: not a build dump");
            std::ptr::null_mut()
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_build_load(path: *const c_char) -> *mut NmBuild {
    let Some(p) = (unsafe { text(path) }) else {
        fail(NM_E_ARG, "nm_build_load: no path");
        return std::ptr::null_mut();
    };
    match std::fs::read_to_string(p) {
        Ok(t) => match std::ffi::CString::new(t) {
            Ok(c) => unsafe { nm_build_from_text(c.as_ptr()) },
            Err(_) => {
                fail(NM_E_PARSE, "nm_build_load: NUL in file");
                std::ptr::null_mut()
            }
        },
        Err(e) => {
            fail(NM_E_IO, format!("{p}: {e}"));
            std::ptr::null_mut()
        }
    }
}

/// Part `i`'s joint with its parent (the root's: its centre), world, in the build's pose.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_build_part_joint(b: *const NmBuild, i: i32, pos: *mut f32) -> i32 {
    let Some(b) = (unsafe { b.as_ref() }) else { return fail(NM_E_ARG, "null build") };
    let (Some(p), Some(f)) = (b.build.parts.get(i as usize), b.frames.get(i as usize)) else { return fail(NM_E_ARG, "no such part") };
    let at = if p.parent.is_some() { f.translation + f.rotation * p.anchor } else { f.translation };
    unsafe { put3(pos, at) };
    0
}

/// `build` kept in its own pose, each part's bone (to its children's joints) stretched by `len[i]` (21 floats), and
/// girth and masses scaled so the whole body weighs `mass` times the original. Unlike nm_build_fit it doesn't take
/// another skeleton's pose.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_build_scale(build: *const NmBuild, len: *const f32, mass: f32) -> *mut NmBuild {
    let Some(b) = (unsafe { build.as_ref() }) else {
        fail(NM_E_ARG, "nm_build_scale: null build");
        return std::ptr::null_mut();
    };
    let n = b.build.parts.len();
    if len.is_null() || !(mass.is_finite() && mass > 0.0) {
        fail(NM_E_ARG, "nm_build_scale: no lengths, or mass <= 0");
        return std::ptr::null_mut();
    }
    let l = unsafe { std::slice::from_raw_parts(len, n) };
    if l.iter().any(|x| !(x.is_finite() && *x > 0.0)) {
        fail(NM_E_ARG, "nm_build_scale: lengths must be > 0");
        return std::ptr::null_mut();
    }
    let (build, frames) = b.build.scaled(&b.frames, l, mass);
    Box::into_raw(Box::new(NmBuild { build, frames }))
}

/// Writes the build as a dump `nm_build_load` and the room read back. Mass overrides (size, heft) aren't in the
/// format: a loaded copy weighs the character file's masses.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_build_save(b: *const NmBuild, path: *const c_char) -> i32 {
    let Some(b) = (unsafe { b.as_ref() }) else { return fail(NM_E_ARG, "null build") };
    let Some(p) = (unsafe { text(path) }) else { return fail(NM_E_ARG, "nm_build_save: no path") };
    match std::fs::write(p, b.build.dump(&b.frames)) {
        Ok(()) => 0,
        Err(e) => fail(NM_E_IO, format!("{p}: {e}")),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_build_destroy(b: *mut NmBuild) {
    if !b.is_null() {
        drop(unsafe { Box::from_raw(b) });
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_build_part_count(b: *const NmBuild) -> i32 {
    unsafe { b.as_ref() }.map_or_else(|| fail(NM_E_ARG, "null build"), |b| b.build.parts.len() as i32)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_build_part_info(b: *const NmBuild, i: i32, mass: *mut f32, radius: *mut f32, half: *mut f32, parent: *mut i32) -> i32 {
    let Some(p) = (unsafe { b.as_ref() }).and_then(|b| b.build.parts.get(i as usize)) else { return fail(NM_E_ARG, "no such part") };
    let m = delirium::nmrs::MASSES[unsafe { &*b }.build.nm].get().and_then(|m| m.get(i as usize)).copied().unwrap_or(p.mass) * p.size.powi(3) * p.heft;
    unsafe {
        for (o, v) in [(mass, m), (radius, p.radius), (half, p.half)] {
            if !o.is_null() {
                *o = v;
            }
        }
        if !parent.is_null() {
            *parent = p.parent.map_or(-1, |j| j as i32);
        }
    }
    0
}

/// Sets part `i`'s mass to `kg` (inertia follows), keeping its shape: for a fitted build whose cubed length scale
/// overweights some parts. Agents made from the build afterwards use it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_build_set_mass(b: *mut NmBuild, i: i32, kg: f32) -> i32 {
    let Some(b) = (unsafe { b.as_mut() }) else { return fail(NM_E_ARG, "null build") };
    if !(kg.is_finite() && kg > 0.0) {
        return fail(NM_E_ARG, "nm_build_set_mass: mass must be > 0");
    }
    let nm = b.build.nm;
    let Some(p) = b.build.parts.get_mut(i as usize) else { return fail(NM_E_ARG, "no such part") };
    let now = delirium::nmrs::MASSES[nm].get().and_then(|m| m.get(i as usize)).copied().unwrap_or(p.mass) * p.size.powi(3) * p.heft;
    p.heft *= kg / now.max(1e-4);
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_build_fit(build: *const NmBuild, joints: *const f32) -> *mut NmBuild {
    let Some(b) = (unsafe { build.as_ref() }) else {
        fail(NM_E_ARG, "nm_build_fit: null build");
        return std::ptr::null_mut();
    };
    let n = b.build.parts.len();
    if joints.is_null() || n != 21 {
        fail(NM_E_ARG, "nm_build_fit: no joints, or not a 21-part build");
        return std::ptr::null_mut();
    }
    let j: Vec<Vec3> = unsafe { std::slice::from_raw_parts(joints, n * 3) }.chunks(3).map(|c| Vec3::new(c[0], c[1], c[2])).collect();
    let bad = j.iter().any(|v| !v.is_finite())
        || (1..n).any(|c| b.build.parts[c].parent.is_some_and(|p| j[c].distance(j[p]) < 1e-3));
    if bad {
        fail(NM_E_ARG, "nm_build_fit: joints not finite, or a joint on its parent's");
        return std::ptr::null_mut();
    }
    let (build, frames) = b.build.fit(&b.frames, &j);
    Box::into_raw(Box::new(NmBuild { build, frames }))
}

/// Rebuilds a 21-part build on another skeleton's bind pose: `joints` (21 x xyz, world) is where each part's joint
/// goes (the root's: its origin), `rots` (21 x quaternion xyzw) how each part is turned. A part with one child is
/// stretched to the skeleton's joint distance; with bit (1 << part) set in `align` it is also turned on until its
/// joint -> child joint line runs along the skeleton's. Shapes, joint limits and muscles stay in each part's own frame. Returns a new build (free with nm_build_destroy) or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_build_remap(build: *const NmBuild, joints: *const f32, rots: *const f32, align: u32) -> *mut NmBuild {
    let Some(b) = (unsafe { build.as_ref() }) else {
        fail(NM_E_ARG, "nm_build_remap: null build");
        return std::ptr::null_mut();
    };
    let n = b.build.parts.len();
    if joints.is_null() || rots.is_null() || n != 21 {
        fail(NM_E_ARG, "nm_build_remap: no joints or rotations, or not a 21-part build");
        return std::ptr::null_mut();
    }
    let j: Vec<Vec3> = unsafe { std::slice::from_raw_parts(joints, n * 3) }.chunks(3).map(|c| Vec3::new(c[0], c[1], c[2])).collect();
    let r: Vec<Quat> = unsafe { std::slice::from_raw_parts(rots, n * 4) }.chunks(4).map(|c| Quat::from_xyzw(c[0], c[1], c[2], c[3])).collect();
    let bad = j.iter().any(|v| !v.is_finite())
        || r.iter().any(|q| !q.is_finite() || q.length() < 0.5)
        || (1..n).any(|c| b.build.parts[c].parent.is_some_and(|p| j[c].distance(j[p]) < 1e-3));
    if bad {
        fail(NM_E_ARG, "nm_build_remap: joints or rotations not finite, or a joint on its parent's");
        return std::ptr::null_mut();
    }
    let (build, frames) = b.build.remap(&j, &r, align);
    Box::into_raw(Box::new(NmBuild { build, frames }))
}

/// Moves part i's joint limits on by `rot` (quaternion xyzw: a turn of the part in its parent's frame), so they are
/// centred on a stance the body wasn't built for (Build::turn_limit). Agents made from the build afterwards use it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_build_turn_limit(b: *mut NmBuild, i: i32, rot: *const f32) -> i32 {
    let Some(b) = (unsafe { b.as_mut() }) else { return fail(NM_E_ARG, "null build") };
    if rot.is_null() {
        return fail(NM_E_ARG, "nm_build_turn_limit: no rotation");
    }
    let q = unsafe { Quat::from_xyzw(*rot, *rot.add(1), *rot.add(2), *rot.add(3)) };
    if !q.is_finite() || q.length() < 1e-6 {
        return fail(NM_E_ARG, "nm_build_turn_limit: not a rotation");
    }
    if i < 0 || !b.build.turn_limit(i as usize, q) {
        return fail(NM_E_ARG, "nm_build_turn_limit: no such part, or the root (no joint)");
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_build_part_frame(b: *const NmBuild, i: i32, pos: *mut f32, rot: *mut f32) -> i32 {
    let Some(f) = (unsafe { b.as_ref() }).and_then(|b| b.frames.get(i as usize)) else { return fail(NM_E_ARG, "no such part") };
    unsafe {
        put3(pos, f.translation);
        put4(rot, f.rotation);
    }
    0
}

pub struct NmAgent {
    agent: Agent,
    build: Build,
    mode: i32,
    set: Vec<bool>,
    pending: Option<(String, Vec<(String, Value)>)>,
    helper: Option<(std::sync::Arc<helper::Net>, helper::State)>,
    /// this agent's own random numbers for hit tasks (None: the shared ones; set by nm_agent_seed_tasks)
    rand: Option<delirium_data::rng::Rand>,
    /// what else is in the world besides the static mesh and the boxes: capsules it is pushed by (nm_agent_set_others)
    others: Vec<delirium::ragdoll::Capsule>,
    /// stable for its life: keys its held contacts with other agents in a group (nm_group_step)
    id: usize,
}

static NEXT_ID: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);

/// the random numbers a hit task draws from: the agent's own if seeded, else the shared ones
fn with_rand<R>(a: &mut Option<delirium_data::rng::Rand>, f: impl FnOnce(&mut delirium_data::rng::Rand) -> R) -> R {
    match a {
        Some(r) => f(r),
        None => f(&mut RAND.lock().unwrap_or_else(|e| e.into_inner())),
    }
}

/// Gives the agent its own random numbers for hit tasks (shots, flinches), so its reactions repeat whatever other
/// agents do on other threads. seed 0 goes back to the shared ones.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_agent_seed_tasks(a: *mut NmAgent, seed: u32) -> i32 {
    let Some(a) = (unsafe { a.as_mut() }) else { return fail(NM_E_ARG, "null agent") };
    a.rand = (seed != 0).then_some(delirium_data::rng::Rand { seed });
    0
}

fn placed(b: &NmBuild, pos: Vec3, yaw: f32) -> Vec<Transform> {
    let floor = b.build.parts.iter().zip(&b.frames).map(|(p, f)| f.translation.z - p.radius - p.half).fold(f32::MAX, f32::min);
    let r = b.frames[0].translation.with_z(floor);
    let turn = Quat::from_rotation_z(yaw);
    b.frames.iter().map(|f| Transform::from_translation(pos + turn * (f.translation - r)).with_rotation(turn * f.rotation)).collect()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_agent_create(b: *const NmBuild, pos: *const f32, yaw: f32, seed: f32) -> *mut NmAgent {
    let Some(b) = (unsafe { b.as_ref() }) else {
        fail(NM_E_ARG, "null build");
        return std::ptr::null_mut();
    };
    let pos = if pos.is_null() { Vec3::ZERO } else { unsafe { v3(pos) } };
    let frames = placed(b, pos, yaw);
    let agent = Agent::new(&b.build, Ragdoll::new(&b.build, &frames, Vec3::ZERO), Quat::from_rotation_z(yaw), seed);
    let n = frames.len();
    Box::into_raw(Box::new(NmAgent { agent, build: b.build.clone(), mode: NM_MODE_LEAD, set: vec![false; n], pending: None, helper: None, rand: None,
        others: vec![], id: NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) }))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_agent_destroy(a: *mut NmAgent) {
    if !a.is_null() {
        drop(unsafe { Box::from_raw(a) });
    }
}

macro_rules! agent {
    ($a:expr) => {
        match unsafe { $a.as_mut() } {
            Some(a) => a,
            None => return fail(NM_E_ARG, "null agent"),
        }
    };
}
macro_rules! agent_ref {
    ($a:expr) => {
        match unsafe { $a.as_ref() } {
            Some(a) => a,
            None => return fail(NM_E_ARG, "null agent"),
        }
    };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_agent_step(a: *mut NmAgent, w: *const NmWorld, dt: f32) -> i32 {
    let a = agent!(a);
    let Some(w) = (unsafe { w.as_ref() }) else { return fail(NM_E_ARG, "null world") };
    if !(dt > 0.0 && dt <= 0.25) {
        return fail(NM_E_ARG, "dt out of (0, 0.25]");
    }
    if a.mode == NM_MODE_FOLLOW && !a.set.iter().all(|&s| s) {
        return fail(NM_E_FOLLOW, "FOLLOW: set every part's state (nm_agent_set_part_state[s]) before each step");
    }
    if a.mode == NM_MODE_BODY {
        // a new agent's ragdoll starts "fresh" (no ground contacts) until its agent steps it once; nothing else clears it here
        a.agent.doll.fresh = false;
        a.agent.doll.step(&a.build, &w.0, dt, Default::default(), &w.1, &a.others);
    } else if a.mode == NM_MODE_FOLLOW {
        if let Some((net, st)) = a.helper.as_mut() {
            helper::tick(net, &net.w, &mut a.agent, &w.0, st, dt);
        }
        a.agent.step_follow(&a.build, &w.0, dt, &w.1, &a.others);
    } else {
        if let Some((net, st)) = a.helper.as_mut() {
            helper::tick(net, &net.w, &mut a.agent, &w.0, st, dt);
        }
        a.agent.step(&a.build, &w.0, dt, &w.1, &a.others);
    }
    a.set.fill(false);
    0
}

/// Sets the capsules the agent is pushed by from its next step on (one-way: they don't move): `count` x 11 floats,
/// end a xyz, end b xyz, radius, velocity xyz, and 1 when it is a body (another ragdoll's part: the overlap is shared,
/// half pushed out each side) or 0 (solid: a character controller). Same frame as the agent's parts. count 0 clears.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_agent_set_others(a: *mut NmAgent, caps: *const f32, count: i32) -> i32 {
    let a = agent!(a);
    if count < 0 || (count > 0 && caps.is_null()) {
        return fail(NM_E_ARG, "nm_agent_set_others: bad array");
    }
    let f = if count == 0 { &[][..] } else { unsafe { std::slice::from_raw_parts(caps, count as usize * 11) } };
    a.others = f.chunks(11).map(|c| (Vec3::new(c[0], c[1], c[2]), Vec3::new(c[3], c[4], c[5]), c[6].max(0.0), Vec3::new(c[7], c[8], c[9]), c[10] != 0.0)).collect();
    0
}

/// The agent's parts as capsules, in nm_agent_set_others's layout (11 floats each, the body flag 1), for another agent
/// to be pushed by. Writes at most `max`; returns the part count.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_agent_get_capsules(a: *const NmAgent, out: *mut f32, max: i32) -> i32 {
    let a = agent_ref!(a);
    let d = &a.agent.doll;
    let n = a.build.parts.len();
    if !out.is_null() && max > 0 {
        let o = unsafe { std::slice::from_raw_parts_mut(out, max.min(n as i32) as usize * 11) };
        for ((k, (t, p)), c) in d.capsules().zip(&a.build.parts).enumerate().zip(o.chunks_mut(11)) {
            let ax = t.rotation * Vec3::Y * p.half;
            let (e0, e1, v) = (t.translation - ax, t.translation + ax, d.motion(k).0);
            c.copy_from_slice(&[e0.x, e0.y, e0.z, e1.x, e1.y, e1.z, p.radius, v.x, v.y, v.z, 1.0]);
        }
    }
    n as i32
}

pub struct NmGroup(delirium::ragdoll::Group);

/// A group: agents stepped together in one world (nm_group_step), so their bodies push each other, held contacts and
/// all. Keeps the contacts between its agents from step to step.
#[unsafe(no_mangle)]
pub extern "C" fn nm_group_create() -> *mut NmGroup {
    Box::into_raw(Box::new(NmGroup(Default::default())))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_group_destroy(g: *mut NmGroup) {
    if !g.is_null() {
        drop(unsafe { Box::from_raw(g) });
    }
}

/// Steps `count` LEAD agents together by `dt` in world `w`: one solve for all of them, where their bodies touch. Each
/// also meets its own capsules (nm_agent_set_others). Their parts must all be in one frame (made and placed relative to
/// the same origin). An agent in another mode fails the call (step it with nm_agent_step). Instead of nm_agent_step
/// for these agents, once each.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_group_step(g: *mut NmGroup, w: *const NmWorld, agents: *mut *mut NmAgent, count: i32, dt: f32) -> i32 {
    let Some(g) = (unsafe { g.as_mut() }) else { return fail(NM_E_ARG, "null group") };
    let Some(w) = (unsafe { w.as_ref() }) else { return fail(NM_E_ARG, "null world") };
    if !(dt > 0.0 && dt <= 0.25) {
        return fail(NM_E_ARG, "dt out of (0, 0.25]");
    }
    if count < 0 || (count > 0 && agents.is_null()) {
        return fail(NM_E_ARG, "nm_group_step: bad array");
    }
    let ptrs = if count == 0 { &[][..] } else { unsafe { std::slice::from_raw_parts(agents, count as usize) } };
    let mut all: Vec<&mut NmAgent> = Vec::with_capacity(ptrs.len());
    for (i, &p) in ptrs.iter().enumerate() {
        if p.is_null() || ptrs[..i].contains(&p) {
            return fail(NM_E_ARG, "nm_group_step: null or repeated agent");
        }
        let a = unsafe { &mut *p };
        if a.mode != NM_MODE_LEAD {
            return fail(NM_E_STATE, "nm_group_step: LEAD agents only");
        }
        all.push(a);
    }
    for a in all.iter_mut() {
        if let Some((net, st)) = a.helper.as_mut() {
            helper::tick(net, &net.w, &mut a.agent, &w.0, st, dt);
        }
    }
    let mut members: Vec<(&mut Agent, usize, &Build, &[Obstacle], &[delirium::ragdoll::Capsule])> =
        all.iter_mut().map(|a| { let a = &mut **a; (&mut a.agent, a.id, &a.build, &w.1[..], &a.others[..]) }).collect();
    Agent::step_group(&mut members, &w.0, dt, &mut g.0);
    drop(members);
    all.iter_mut().for_each(|a| a.set.fill(false));
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_agent_set_mode(a: *mut NmAgent, mode: i32) -> i32 {
    let a = agent!(a);
    if mode != NM_MODE_LEAD && mode != NM_MODE_FOLLOW && mode != NM_MODE_BODY {
        return fail(NM_E_ARG, "mode is NM_MODE_LEAD, NM_MODE_FOLLOW or NM_MODE_BODY");
    }
    a.mode = mode;
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_agent_get_mode(a: *const NmAgent) -> i32 {
    agent_ref!(a).mode
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_agent_part_count(a: *const NmAgent) -> i32 {
    agent_ref!(a).build.parts.len() as i32
}

fn part(a: &NmAgent, i: i32) -> Option<usize> {
    (0..a.build.parts.len() as i32).contains(&i).then_some(i as usize)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_agent_get_part_transform(a: *const NmAgent, i: i32, pos: *mut f32, rot: *mut f32) -> i32 {
    let a = agent_ref!(a);
    let Some(k) = part(a, i) else { return fail(NM_E_ARG, "no such part") };
    let f = a.agent.doll.frames().nth(k).unwrap_or_default();
    unsafe {
        put3(pos, f.translation);
        put4(rot, f.rotation);
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_agent_get_part_transforms(a: *const NmAgent, out: *mut f32, parts: i32) -> i32 {
    let a = agent_ref!(a);
    if out.is_null() {
        return fail(NM_E_ARG, "null out");
    }
    let mut n = 0;
    for (k, f) in a.agent.doll.frames().enumerate().take(parts.max(0) as usize) {
        unsafe {
            put3(out.add(k * 7), f.translation);
            put4(out.add(k * 7 + 3), f.rotation);
        }
        n += 1;
    }
    n
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_agent_get_part_velocity(a: *const NmAgent, i: i32, vel: *mut f32, angvel: *mut f32) -> i32 {
    let a = agent_ref!(a);
    let Some(k) = part(a, i) else { return fail(NM_E_ARG, "no such part") };
    let (v, w) = a.agent.doll.motion(k);
    unsafe {
        put3(vel, v);
        put3(angvel, w);
    }
    0
}

/// Part `i`'s world inverse inertia (9 floats, row-major) and inverse mass: what a host needs to turn the joint
/// accelerations into its own bodies' spins the way the plugin does, when the host is a test body (NM_MODE_BODY).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_agent_get_part_inv_inertia(a: *const NmAgent, i: i32, inv: *mut f32, inv_mass: *mut f32) -> i32 {
    let a = agent_ref!(a);
    let Some(k) = part(a, i) else { return fail(NM_E_ARG, "no such part") };
    if inv.is_null() {
        return fail(NM_E_ARG, "null out");
    }
    let m = a.agent.doll.tensor(k).inverse().as_mat3();
    unsafe {
        for r in 0..3 {
            for c in 0..3 {
                *inv.add(r * 3 + c) = m.row(r)[c];
            }
        }
        if !inv_mass.is_null() {
            *inv_mass = 1.0 / a.agent.doll.part_mass(k).max(1e-6);
        }
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_agent_set_part_state(a: *mut NmAgent, i: i32, pos: *const f32, rot: *const f32, vel: *const f32, angvel: *const f32) -> i32 {
    let a = agent!(a);
    let Some(k) = part(a, i) else { return fail(NM_E_ARG, "no such part") };
    if [pos, rot, vel, angvel].iter().any(|p| p.is_null()) {
        return fail(NM_E_ARG, "null vector");
    }
    let q = unsafe { Quat::from_xyzw(*rot, *rot.add(1), *rot.add(2), *rot.add(3)) };
    a.agent.doll.set_body(k, unsafe { v3(pos) }, q, unsafe { v3(vel) }, unsafe { v3(angvel) });
    a.set[k] = true;
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_agent_set_part_states(a: *mut NmAgent, states: *const f32, parts: i32) -> i32 {
    if states.is_null() {
        return fail(NM_E_ARG, "null states");
    }
    for k in 0..parts.max(0) as usize {
        let s = unsafe { states.add(k * 13) };
        let r = unsafe { nm_agent_set_part_state(a, k as i32, s, s.add(3), s.add(7), s.add(10)) };
        if r < 0 {
            return r;
        }
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_agent_set_incoming_transforms(a: *mut NmAgent, transforms: *const f32, parts: i32) -> i32 {
    let a = agent!(a);
    let n = parts.max(0) as usize;
    if n > 0 && (transforms.is_null() || n != a.agent.doll.frames().count()) {
        return fail(NM_E_ARG, "need every part's transform");
    }
    a.agent.incoming = (0..n).map(|k| unsafe {
        let s = transforms.add(k * 7);
        Transform { translation: v3(s), rotation: Quat::from_xyzw(*s.add(3), *s.add(4), *s.add(5), *s.add(6)), ..Default::default() }
    }).collect();
    0
}

/// The pose the agent's muscles and balance hold (its "zero pose"), from every part's world transform (7 floats each:
/// position, quaternion xyzw). By default it is the build's own pose; a host game sets its animation's pose here, once
/// or every frame, so the body keeps that posture instead of the build's.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_agent_set_zero_pose(a: *mut NmAgent, transforms: *const f32, parts: i32) -> i32 {
    let a = agent!(a);
    let n = parts.max(0) as usize;
    if transforms.is_null() || n != a.agent.doll.frames().count() {
        return fail(NM_E_ARG, "need every part's transform");
    }
    let frames: Vec<Transform> = (0..n).map(|k| unsafe {
        let s = transforms.add(k * 7);
        Transform { translation: v3(s), rotation: Quat::from_xyzw(*s.add(3), *s.add(4), *s.add(5), *s.add(6)).normalize(), ..Default::default() }
    }).collect();
    a.agent.set_zero(&a.build, &frames);
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_agent_get_part_drive(a: *const NmAgent, i: i32, force: *mut f32, torque: *mut f32) -> i32 {
    let a = agent_ref!(a);
    let Some(k) = part(a, i) else { return fail(NM_E_ARG, "no such part") };
    let (f, t) = a.agent.doll.out[k];
    unsafe {
        put3(force, f);
        put3(torque, t);
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_agent_get_part_drives(a: *const NmAgent, out: *mut f32, parts: i32) -> i32 {
    let a = agent_ref!(a);
    if out.is_null() {
        return fail(NM_E_ARG, "null out");
    }
    let n = (parts.max(0) as usize).min(a.agent.doll.out.len());
    for (k, (f, t)) in a.agent.doll.out.iter().take(n).enumerate() {
        unsafe {
            put3(out.add(k * 6), *f);
            put3(out.add(k * 6 + 3), *t);
        }
    }
    n as i32
}

/// Every part's joint acceleration from its muscles (3 floats each, world, rad/s^2): how fast the muscles turn the
/// part against its parent through the whole body. A FOLLOW host whose own bodies have other masses applies this as a
/// change of the joint's relative spin (times its step) rather than the torques. The root's is zero.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_agent_get_joint_accels(a: *const NmAgent, out: *mut f32, parts: i32) -> i32 {
    let a = agent_ref!(a);
    if out.is_null() {
        return fail(NM_E_ARG, "null out");
    }
    let n = (parts.max(0) as usize).min(a.agent.doll.joint_accel.len());
    for (k, v) in a.agent.doll.joint_accel.iter().take(n).enumerate() {
        unsafe { put3(out.add(k * 3), *v) };
    }
    n as i32
}

/// Every part's torque from behaviours acting on the part itself (3 floats each, world, N m): a shot's spin, electrocute's
/// jolts. Not the muscles' (nm_agent_get_joint_accels): a FOLLOW host turns its own body by this on top of them.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_agent_get_part_ext_torques(a: *const NmAgent, out: *mut f32, parts: i32) -> i32 {
    let a = agent_ref!(a);
    if out.is_null() {
        return fail(NM_E_ARG, "null out");
    }
    let n = (parts.max(0) as usize).min(a.agent.doll.ext_torque.len());
    for (k, v) in a.agent.doll.ext_torque.iter().take(n).enumerate() {
        unsafe { put3(out.add(k * 3), *v) };
    }
    n as i32
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_agent_get_part_contact(a: *const NmAgent, i: i32) -> i32 {
    let a = agent_ref!(a);
    let Some(k) = part(a, i) else { return fail(NM_E_ARG, "no such part") };
    a.agent.doll.touch[k] as i32
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_agent_get_com(a: *const NmAgent, pos: *mut f32, vel: *mut f32) -> i32 {
    let a = agent_ref!(a);
    let (x, v) = a.agent.doll.com();
    unsafe {
        put3(pos, x);
        put3(vel, v);
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_agent_get_mass(a: *const NmAgent) -> f32 {
    unsafe { a.as_ref() }.map_or(0.0, |a| a.agent.doll.mass())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_agent_get_kinetic_energy(a: *const NmAgent) -> f32 {
    unsafe { a.as_ref() }.map_or(0.0, |a| a.agent.doll.kinetic())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_agent_apply_impulse(a: *mut NmAgent, i: i32, at: *const f32, impulse: *const f32) -> i32 {
    let a = agent!(a);
    let Some(k) = part(a, i) else { return fail(NM_E_ARG, "no such part") };
    if at.is_null() || impulse.is_null() {
        return fail(NM_E_ARG, "null vector");
    }
    a.agent.doll.impulse(k, unsafe { v3(at) }, unsafe { v3(impulse) });
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_agent_apply_push(a: *mut NmAgent, at: *const f32, impulse: *const f32) -> i32 {
    let a = agent!(a);
    if at.is_null() || impulse.is_null() {
        return fail(NM_E_ARG, "null vector");
    }
    a.agent.doll.push(&a.build, unsafe { v3(at) }, unsafe { v3(impulse) });
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_agent_nearest_part(a: *const NmAgent, at: *const f32) -> i32 {
    let a = agent_ref!(a);
    if at.is_null() {
        return fail(NM_E_ARG, "null vector");
    }
    a.agent.doll.nearest(&a.build, unsafe { v3(at) }) as i32
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_agent_raycast(a: *const NmAgent, from: *const f32, to: *const f32, point: *mut f32, normal: *mut f32) -> i32 {
    let a = agent_ref!(a);
    if from.is_null() || to.is_null() {
        return fail(NM_E_ARG, "null vector");
    }
    match a.agent.doll.probe(&a.build, unsafe { v3(from) }, unsafe { v3(to) }) {
        Some((k, p, n)) => {
            unsafe {
                put3(point, p);
                put3(normal, n);
            }
            k as i32
        }
        None => -1,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_agent_add_velocity(a: *mut NmAgent, dv: *const f32) -> i32 {
    let a = agent!(a);
    if dv.is_null() {
        return fail(NM_E_ARG, "null vector");
    }
    a.agent.doll.kick(unsafe { v3(dv) });
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_agent_get_joint(a: *const NmAgent, i: i32, parent: *mut i32, pivot: *mut f32, frame: *mut f32, limits: *mut f32,
                                            armature: *mut f32) -> i32 {
    let a = agent_ref!(a);
    let Some(k) = part(a, i) else { return fail(NM_E_ARG, "no such part") };
    let Some(j) = a.agent.joints(&a.build)[k] else { return 0 };
    let pf = a.agent.doll.frames().nth(j.parent).unwrap_or_default();
    unsafe {
        if !parent.is_null() {
            *parent = j.parent as i32;
        }
        put3(pivot, pf.translation + pf.rotation * j.anchor_parent);
        put4(frame, pf.rotation * j.in_parent);
        put3(limits, Vec3::from_array(j.limits));
        if !armature.is_null() {
            *armature = a.agent.doll.armature.get(k).copied().unwrap_or(0.0);
        }
    }
    if j.hinge { 2 } else { 1 }
}

fn message(m: i32) -> Option<&'static msg::Message> {
    msg::MESSAGES.get(m as usize).filter(|_| m >= 0)
}

fn param(m: i32, p: i32) -> Option<&'static msg::Param> {
    message(m).and_then(|m| m.params.get(p as usize)).filter(|_| p >= 0)
}

#[unsafe(no_mangle)]
pub extern "C" fn nm_message_count() -> i32 {
    msg::MESSAGES.len() as i32
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_message_name(m: i32, buf: *mut c_char, len: i32) -> i32 {
    match message(m) {
        Some(m) => unsafe { put(m.name, buf, len) },
        None => fail(NM_E_MESSAGE, "no such message"),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_message_find(name: *const c_char) -> i32 {
    let n = unsafe { text(name) }.unwrap_or("");
    msg::MESSAGES.iter().position(|m| m.name == n).map_or_else(|| fail(NM_E_MESSAGE, format!("no message {n}")), |i| i as i32)
}

#[unsafe(no_mangle)]
pub extern "C" fn nm_message_param_count(m: i32) -> i32 {
    message(m).map_or_else(|| fail(NM_E_MESSAGE, "no such message"), |m| m.params.len() as i32)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_message_param_name(m: i32, p: i32, buf: *mut c_char, len: i32) -> i32 {
    match param(m, p) {
        Some(p) => unsafe { put(p.name, buf, len) },
        None => fail(NM_E_PARAM, "no such param"),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_message_param_find(m: i32, name: *const c_char) -> i32 {
    let n = unsafe { text(name) }.unwrap_or("");
    message(m).and_then(|m| m.params.iter().position(|p| p.name == n)).map_or_else(|| fail(NM_E_PARAM, format!("no param {n}")), |i| i as i32)
}

pub const NM_KIND_FLOAT: i32 = 0;
pub const NM_KIND_INT: i32 = 1;
pub const NM_KIND_BOOL: i32 = 2;
pub const NM_KIND_VEC3: i32 = 3;
pub const NM_KIND_STRING: i32 = 4;

#[unsafe(no_mangle)]
pub extern "C" fn nm_message_param_kind(m: i32, p: i32) -> i32 {
    param(m, p).map_or_else(|| fail(NM_E_PARAM, "no such param"), |p| match p.kind {
        Kind::Float => NM_KIND_FLOAT,
        Kind::Int => NM_KIND_INT,
        Kind::Bool => NM_KIND_BOOL,
        Kind::Vec3 => NM_KIND_VEC3,
        Kind::Str => NM_KIND_STRING,
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn nm_message_param_used(m: i32, p: i32) -> i32 {
    param(m, p).map_or_else(|| fail(NM_E_PARAM, "no such param"), |p| p.used as i32)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_message_param_doc(m: i32, p: i32, buf: *mut c_char, len: i32) -> i32 {
    match param(m, p) {
        Some(p) => unsafe { put(p.doc, buf, len) },
        None => fail(NM_E_PARAM, "no such param"),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_message_param_default(m: i32, p: i32, out: *mut f32) -> i32 {
    let (Some(mm), Some(pp)) = (message(m), param(m, p)) else { return fail(NM_E_PARAM, "no such param") };
    unsafe { floats(msg::get(&msg::State::default(), mm.name, pp.name), out) }
}

unsafe fn floats(v: Option<Value>, out: *mut f32) -> i32 {
    let Some(v) = v else { return fail(NM_E_STATE, "the port doesn't keep this param") };
    let f: &[f32] = match &v {
        Value::Float(x) => &[*x],
        Value::Int(i) => &[*i as f32],
        Value::Bool(b) => &[*b as i32 as f32],
        Value::Vec3(v) => &v.to_array(),
        Value::Str(_) => &[],
    };
    if !out.is_null() {
        unsafe { std::ptr::copy_nonoverlapping(f.as_ptr(), out, f.len()) };
    }
    f.len() as i32
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_msg_begin(a: *mut NmAgent, name: *const c_char) -> i32 {
    let a = agent!(a);
    let Some(n) = (unsafe { text(name) }) else { return fail(NM_E_ARG, "no name") };
    if msg::find(n).is_none() {
        return fail(NM_E_MESSAGE, format!("no message {n}"));
    }
    a.pending = Some((n.to_string(), vec![]));
    0
}

unsafe fn add(a: *mut NmAgent, name: *const c_char, v: Value) -> i32 {
    let a = agent!(a);
    let Some(n) = (unsafe { text(name) }) else { return fail(NM_E_ARG, "no name") };
    let Some((m, ps)) = a.pending.as_mut() else { return fail(NM_E_STATE, "nm_msg_begin first") };
    if n != "start" && !msg::find(m).is_some_and(|m| m.params.iter().any(|p| p.name == n)) {
        return fail(NM_E_PARAM, format!("{m} has no param {n}"));
    }
    ps.push((n.to_string(), v));
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_msg_set_float(a: *mut NmAgent, name: *const c_char, v: f32) -> i32 {
    unsafe { add(a, name, Value::Float(v)) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_msg_set_int(a: *mut NmAgent, name: *const c_char, v: i32) -> i32 {
    unsafe { add(a, name, Value::Int(v)) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_msg_set_bool(a: *mut NmAgent, name: *const c_char, v: i32) -> i32 {
    unsafe { add(a, name, Value::Bool(v != 0)) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_msg_set_vec3(a: *mut NmAgent, name: *const c_char, v: *const f32) -> i32 {
    if v.is_null() {
        return fail(NM_E_ARG, "null vector");
    }
    unsafe { add(a, name, Value::Vec3(v3(v))) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_msg_set_string(a: *mut NmAgent, name: *const c_char, v: *const c_char) -> i32 {
    let Some(s) = (unsafe { text(v) }) else { return fail(NM_E_ARG, "null string") };
    unsafe { add(a, name, Value::Str(s.to_string())) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_msg_send(a: *mut NmAgent, w: *const NmWorld) -> i32 {
    let a = agent!(a);
    let Some(w) = (unsafe { w.as_ref() }) else { return fail(NM_E_ARG, "null world") };
    let Some((m, ps)) = a.pending.take() else { return fail(NM_E_STATE, "nm_msg_begin first") };
    let ps: Vec<(&str, Value)> = ps.iter().map(|(n, v)| (n.as_str(), v.clone())).collect();
    let mut st = std::mem::take(&mut a.agent.msgs);
    let r = msg::send(&mut a.agent, &a.build, &w.0, &mut st, &m, &ps);
    a.agent.msgs = st;
    match r {
        Ok(n) => n as i32,
        Err(msg::Error::Message) => fail(NM_E_MESSAGE, format!("no message {m}")),
        Err(msg::Error::Param) => fail(NM_E_PARAM, format!("{m}: a param it doesn't take")),
        Err(msg::Error::Type) => fail(NM_E_TYPE, format!("{m}: a param of the wrong type")),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_msg_cancel(a: *mut NmAgent) -> i32 {
    agent!(a).pending = None;
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_agent_get_param(a: *const NmAgent, message: *const c_char, param: *const c_char, out: *mut f32) -> i32 {
    let a = agent_ref!(a);
    let (Some(m), Some(p)) = (unsafe { text(message) }, unsafe { text(param) }) else { return fail(NM_E_ARG, "no name") };
    unsafe { floats(msg::get(&a.agent.msgs, m, p), out) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_agent_get_param_string(a: *const NmAgent, message: *const c_char, param: *const c_char, buf: *mut c_char, len: i32) -> i32 {
    let a = agent_ref!(a);
    let (Some(m), Some(p)) = (unsafe { text(message) }, unsafe { text(param) }) else { return fail(NM_E_ARG, "no name") };
    match msg::get(&a.agent.msgs, m, p) {
        Some(Value::Str(s)) => unsafe { put(&s, buf, len) },
        Some(_) => fail(NM_E_TYPE, "not a string param"),
        None => fail(NM_E_STATE, "the port doesn't keep this param"),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_agent_is_balancing(a: *const NmAgent) -> i32 {
    agent_ref!(a).agent.balancing() as i32
}

/// The balancer's stepping foot: 0 none, 1 left, 2 right; -1 when not balancing.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_agent_balance_stepping(a: *const NmAgent) -> i32 {
    agent_ref!(a).agent.balance_steps().map_or(-1, |(_, s)| s as i32)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_agent_has_fallen(a: *const NmAgent) -> i32 {
    agent_ref!(a).agent.fallen as i32
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_agent_is_settled(a: *const NmAgent) -> i32 {
    agent_ref!(a).agent.settled() as i32
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_agent_is_relaxing(a: *const NmAgent) -> i32 {
    agent_ref!(a).agent.relaxing() as i32
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_agent_is_catching_fall(a: *const NmAgent) -> i32 {
    agent_ref!(a).agent.catching() as i32
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_agent_is_rolled_up(a: *const NmAgent) -> i32 {
    agent_ref!(a).agent.rolled_up() as i32
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_agent_is_awake(a: *const NmAgent) -> i32 {
    agent_ref!(a).agent.doll.awake() as i32
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_agent_get_stance(a: *const NmAgent, pos: *mut f32) -> i32 {
    match agent_ref!(a).agent.stance() {
        Some(p) => {
            unsafe { put3(pos, p) };
            1
        }
        None => 0,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_agent_feedback_count(a: *const NmAgent) -> i32 {
    agent_ref!(a).agent.feedback.len() as i32
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_agent_feedback_get(a: *const NmAgent, i: i32, buf: *mut c_char, len: i32, slot: *mut i32, value: *mut f32) -> i32 {
    let a = agent_ref!(a);
    let Some(f) = a.agent.feedback.get(i as usize).filter(|_| i >= 0) else { return fail(NM_E_ARG, "no such feedback") };
    unsafe {
        if !slot.is_null() {
            *slot = f.slot as i32;
        }
        if !value.is_null() {
            *value = f.value;
        }
        put(f.name, buf, len)
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_agent_task_shot(a: *mut NmAgent, w: *const NmWorld, set: *const c_char, part: i32, at: *const f32, dir: *const f32, health: f32) -> i32 {
    let a = agent!(a);
    let Some(w) = (unsafe { w.as_ref() }) else { return fail(NM_E_ARG, "null world") };
    if at.is_null() || dir.is_null() || !(0..a.build.parts.len() as i32).contains(&part) {
        return fail(NM_E_ARG, "bad hit");
    }
    let set = unsafe { text(set) }.unwrap_or("*");
    let hit = nm::Hit { health, ..nm::Hit::ragdoll(&a.agent) };
    let dir = unsafe { v3(dir) }.normalize_or_zero();
    let at = unsafe { v3(at) };
    with_rand(&mut a.rand, |r| with_tp(|tp| nm::task_shot(&mut a.agent, &a.build, &w.0, tp, r, set, hit, part as usize, at, dir * SHOT_FORCE, -dir, false)));
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_agent_task_balance(a: *mut NmAgent, w: *const NmWorld, set: *const c_char, look: *const f32) -> i32 {
    let a = agent!(a);
    let Some(w) = (unsafe { w.as_ref() }) else { return fail(NM_E_ARG, "null world") };
    let set = unsafe { text(set) }.unwrap_or("*");
    let look = (!look.is_null()).then(|| unsafe { v3(look) });
    with_tp(|tp| nm::task_balance(&mut a.agent, &a.build, &w.0, tp, set, look, false));
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_agent_task_catch_fall(a: *mut NmAgent, set: *const c_char) -> i32 {
    let a = agent!(a);
    let set = unsafe { text(set) }.unwrap_or("*");
    with_tp(|tp| nm::task_catch_fall(&mut a.agent, tp, set));
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_agent_task_flinch(a: *mut NmAgent, w: *const NmWorld, set: *const c_char, from: *const f32) -> i32 {
    let a = agent!(a);
    let Some(w) = (unsafe { w.as_ref() }) else { return fail(NM_E_ARG, "null world") };
    if from.is_null() {
        return fail(NM_E_ARG, "null vector");
    }
    let set = unsafe { text(set) }.unwrap_or("*");
    let from = unsafe { v3(from) };
    with_rand(&mut a.rand, |r| with_tp(|tp| nm::task_flinch(&mut a.agent, &a.build, &w.0, tp, r, set, from)));
    0
}

/// A trained balance helper (helper.rs), shared by any number of agents.
pub struct NmHelper(std::sync::Arc<helper::Net>);

/// Loads a balance helper network (.helper file from the trainer); null on failure (nm_last_error says why).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_helper_load(path: *const c_char) -> *mut NmHelper {
    let Some(p) = (unsafe { text(path) }) else {
        fail(NM_E_ARG, "nm_helper_load: no path");
        return std::ptr::null_mut();
    };
    match helper::Net::load(p) {
        Some(n) => Box::into_raw(Box::new(NmHelper(std::sync::Arc::new(n)))),
        None => {
            fail(NM_E_PARSE, format!("{p}: not a balance helper network"));
            std::ptr::null_mut()
        }
    }
}

/// Agents keep their own reference: destroying the helper does not stop agents already using it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_helper_destroy(h: *mut NmHelper) {
    if !h.is_null() {
        drop(unsafe { Box::from_raw(h) });
    }
}

/// Lets the helper steer this agent's balancer while she balances (null: off, the balancer as built).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_agent_set_helper(a: *mut NmAgent, h: *const NmHelper) -> i32 {
    let a = agent!(a);
    attach_helper(a, unsafe { h.as_ref() }.map(|h| h.0.clone()));
    0
}

/// Tells the helper what just hit her: 1 push / melee / stagger, 2 shot, 3 burn, 4 shock, 5 chill (0 none).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nm_agent_helper_hit(a: *mut NmAgent, kind: i32) -> i32 {
    let a = agent!(a);
    if !(0..helper::HITS as i32).contains(&kind) {
        return fail(NM_E_ARG, "hit kind 0-5");
    }
    if let Some((_, st)) = a.helper.as_mut() {
        st.hit(kind as usize);
    }
    0
}

/// Rust hosts (the trainer): a network of their own, or None.
pub fn attach_helper(a: &mut NmAgent, net: Option<std::sync::Arc<helper::Net>>) {
    a.helper = net.map(|n| (n, helper::State::default()));
    if a.helper.is_none() {
        a.agent.assist = None;
    }
}

/// Rust hosts: the agent's balancer state for the trainer's measurements.
pub fn agent_core(a: &NmAgent) -> &Agent {
    &a.agent
}

/// Rust hosts: what the helper would see now (its own state if attached, else a fresh one).
pub fn helper_observe(a: &NmAgent, w: &NmWorld) -> [f32; helper::OBS] {
    let st = a.helper.as_ref().map_or_else(helper::State::default, |h| h.1);
    helper::observe(&a.agent, &w.0, &st)
}
