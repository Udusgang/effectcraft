//! Reel helpers: reframe a composition to another aspect (9:16 ⇄ 16:9, 1:1, 4:5) in one step,
//! and mark the beats of a music layer so cuts can land on them. Both are built for editing by
//! agents as much as by hand, and each is one undo step.

use effectcraft_project::{ItemId, ItemKind, LayerSource, Marker};
use effectcraft_time::Tick;
use serde_json::{Value, json};

use super::app_more::grouped;
use super::{CommandSpec, always, b_p, bad, comp_id, f_p, has_comp, layer_mut, layer_p, layers_p, resolve_layer, str_p};
use crate::{EngineError, Result, Session, cmd};

// ---------------------------------------------------------------- comp.reframe

/// Frame size of a named aspect (the sizes reels and videos are delivered at).
fn aspect_size(aspect: &str) -> Option<(f64, f64)> {
    Some(match aspect.trim().to_ascii_lowercase().as_str() {
        "9:16" | "vertical" | "reel" | "story" => (1080.0, 1920.0),
        "16:9" | "landscape" | "horizontal" => (1920.0, 1080.0),
        "1:1" | "square" => (1080.0, 1080.0),
        "4:5" | "portrait" => (1080.0, 1350.0),
        _ => return None,
    })
}

/// `comp.reframe`: a new composition of another size holding the source comp, framed by `mode`:
/// `fit` (all of it visible, bars where the shapes differ), `fill` (covers the frame, edges
/// cropped) or `blur` (fit, over a filled, blurred and darkened copy, the usual way to show a
/// 16:9 shot in a 9:16 reel and the reverse).
fn reframe(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "comp.reframe";
    let src = comp_id(s, p)?;
    let comp = s.project.comp(src).ok_or(EngineError::NoComp)?.clone();
    let src_name = s.project.item(src).map(|i| i.name.clone()).unwrap_or_default();
    let (w, h) = match (f_p(p, "width"), f_p(p, "height"), str_p(p, "aspect")) {
        (Some(w), Some(h), _) => (w, h),
        (_, _, Some(a)) => aspect_size(a).ok_or_else(|| bad(C, format!("aspect: 9:16|16:9|1:1|4:5, not `{a}`")))?,
        _ => return Err(bad(C, "give `aspect` (9:16, 16:9, 1:1, 4:5) or `width` and `height`")),
    };
    let size_ok = |v: f64| v.is_finite() && (16.0..=30_000.0).contains(&v);
    if !size_ok(w) || !size_ok(h) {
        return Err(bad(C, "width and height must be 16 to 30000 px"));
    }
    let (w, h) = (w.round(), h.round());
    let mode = str_p(p, "mode").unwrap_or("blur");
    if !matches!(mode, "fit" | "fill" | "blur") {
        return Err(bad(C, format!("mode: fit|fill|blur, not `{mode}`")));
    }
    let blur = f_p(p, "blur").filter(|b| b.is_finite() && (0.0..=1000.0).contains(b)).unwrap_or(60.0);
    let dim = f_p(p, "dim").filter(|d| d.is_finite() && (-150.0..=150.0).contains(d)).unwrap_or(-40.0);
    let name = str_p(p, "name").map(str::to_string).unwrap_or_else(|| format!("{src_name} {}x{}", w as u32, h as u32));
    let (sw, sh) = (f64::from(comp.width.max(1)), f64::from(comp.height.max(1)));
    let fit = (w / sw).min(h / sh) * 100.0;
    let fill = (w / sw).max(h / sh) * 100.0;
    let params = json!({
        "name": name,
        "width": w as u32,
        "height": h as u32,
        "frameRate": comp.frame_rate.as_f64(),
        "duration": comp.duration.seconds(),
        "background": comp.background,
    });
    grouped(s, "Reframe Composition", |s| {
        let new = s.execute("comp.new", params)?["comp"].as_u64().ok_or_else(|| bad(C, "the new composition has no id"))?;
        // Each copy goes on top of the stack, so the background is added first.
        let add = |s: &mut Session, label: String, scale: f64| -> Result<u64> {
            let l = s.execute("layer.addItem", json!({"comp": new, "item": src.0, "time": 0.0, "index": 1}))?["layer"]
                .as_u64()
                .ok_or_else(|| bad(C, "the new layer has no id"))?;
            s.execute("prop.set", json!({"comp": new, "layer": l, "path": "transform/scale", "value": [scale, scale, 100.0]}))?;
            s.execute("layer.settings", json!({"comp": new, "layer": l, "name": label}))?;
            Ok(l)
        };
        let background = if mode == "blur" {
            let b = add(s, format!("{src_name} (blurred background)"), fill)?;
            s.execute("effect.apply", json!({"comp": new, "effect": "ec.blur.gaussian", "layers": [b]}))?;
            s.execute("prop.set", json!({"comp": new, "layer": b, "path": "effects/#1/blurriness", "value": blur}))?;
            s.execute("prop.set", json!({"comp": new, "layer": b, "path": "effects/#1/repeatEdge", "value": true}))?;
            s.execute("effect.apply", json!({"comp": new, "effect": "ec.color.brightnesscontrast", "layers": [b]}))?;
            s.execute("prop.set", json!({"comp": new, "layer": b, "path": "effects/#2/brightness", "value": dim}))?;
            Some(b)
        } else {
            None
        };
        let front = add(s, src_name.clone(), if mode == "fill" { fill } else { fit })?;
        Ok(
            json!({"comp": new, "front": front, "background": background, "mode": mode, "width": w, "height": h, "scale": if mode == "fill" { fill } else { fit }}),
        )
    })
}

// ---------------------------------------------------------------- audio.detectBeats

/// Analysis rate: plenty for beats, a quarter of the work of 48 kHz.
const RATE: u32 = 22_050;
/// Samples per onset-envelope step (≈23 ms).
const HOP: usize = 512;
/// Longest stretch analysed in one call (15 minutes).
const MAX_SECONDS: f64 = 900.0;

/// Onset strength per hop: the rise of log energy, with a low band (kick drums) weighted over
/// the full band (snares, claps, vocals).
pub(crate) fn onset_envelope(mono: &[f32]) -> Vec<f64> {
    // One-pole low-pass at ≈150 Hz.
    let a = 1.0 - (-2.0 * std::f32::consts::PI * 150.0 / RATE as f32).exp();
    let mut lp = 0.0f32;
    let energies: Vec<(f64, f64)> = mono
        .chunks(HOP)
        .map(|chunk| {
            let (mut full, mut low) = (0.0f64, 0.0f64);
            for &x in chunk {
                let x = if x.is_finite() { x } else { 0.0 };
                lp += a * (x - lp);
                full += f64::from(x * x);
                low += f64::from(lp * lp);
            }
            let n = chunk.len().max(1) as f64;
            (full / n, low / n)
        })
        .collect();
    // A floor relative to the loudness of the piece, so near-silence doesn't produce huge log
    // jumps.
    let mean = |f: fn(&(f64, f64)) -> f64| energies.iter().map(f).sum::<f64>() / energies.len().max(1) as f64;
    let (floor_full, floor_low) = ((mean(|e| e.0) * 1e-3).max(1e-12), (mean(|e| e.1) * 1e-3).max(1e-12));
    let logs: Vec<(f64, f64)> = energies.iter().map(|(f, l)| (f.max(floor_full).ln(), l.max(floor_low).ln())).collect();
    let mut env = vec![0.0; logs.len()];
    for (i, w) in logs.windows(2).enumerate() {
        if let (Some(v), [prev, cur]) = (env.get_mut(i + 1), w) {
            *v = (cur.0 - prev.0).max(0.0) + 2.0 * (cur.1 - prev.1).max(0.0);
        }
    }
    env
}

/// Hops between beats → beats per minute.
fn bpm_of(lag: f64) -> f64 {
    60.0 * RATE as f64 / (lag * HOP as f64)
}

/// Tempo from the envelope's autocorrelation over 60–200 BPM, nudged towards 120 BPM so a
/// half- or double-time reading doesn't win on a near tie. Returns (bpm, period in hops,
/// confidence 0–1).
pub(crate) fn estimate_tempo(env: &[f64]) -> Option<(f64, f64, f64)> {
    let mean = env.iter().sum::<f64>() / env.len().max(1) as f64;
    let x: Vec<f64> = env.iter().map(|v| v - mean).collect();
    let energy: f64 = x.iter().map(|v| v * v).sum();
    if energy <= 0.0 {
        return None;
    }
    let (lo, hi) = ((60.0 * RATE as f64 / (200.0 * HOP as f64)).floor() as usize, (60.0 * RATE as f64 / (60.0 * HOP as f64)).ceil() as usize);
    let mut best: Option<(f64, usize, f64)> = None;
    for lag in lo.max(1)..=hi {
        if lag >= x.len() {
            break;
        }
        let r: f64 = x.iter().zip(x.iter().skip(lag)).map(|(a, b)| a * b).sum::<f64>() / energy;
        let prior = (-0.5 * ((bpm_of(lag as f64) / 120.0).log2() / 0.9).powi(2)).exp();
        let score = r * prior;
        if best.is_none_or(|(s, _, _)| score > s) {
            best = Some((score, lag, r));
        }
    }
    let (_, lag, r) = best?;
    // Refine the period with a parabola through the neighbouring lags.
    let ac = |l: usize| x.iter().zip(x.iter().skip(l)).map(|(a, b)| a * b).sum::<f64>();
    let (y0, y1, y2) = (ac(lag.saturating_sub(1)), ac(lag), ac(lag + 1));
    let denom = y0 - 2.0 * y1 + y2;
    let offset = if denom.abs() > 1e-12 { (0.5 * (y0 - y2) / denom).clamp(-0.5, 0.5) } else { 0.0 };
    let period = lag as f64 + offset;
    Some((bpm_of(period), period, r.clamp(0.0, 1.0)))
}

/// Onsets: local maxima of the envelope at least `min_gap` hops apart that stand above the
/// local average by a margin `sensitivity` (0–1) lowers.
pub(crate) fn pick_onsets(env: &[f64], min_gap: usize, sensitivity: f64) -> Vec<usize> {
    let n = env.len();
    if n == 0 {
        return vec![];
    }
    let mean = env.iter().sum::<f64>() / n as f64;
    let std = (env.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n as f64).sqrt();
    let peak = env.iter().copied().fold(0.0, f64::max);
    let margin = (1.0 - sensitivity.clamp(0.0, 1.0)) * 2.0 * std;
    let half = (RATE as usize * 2 / 5 / HOP).max(1); // ±0.4 s local average
    let radius = (min_gap / 2).max(1);
    let mut out: Vec<usize> = vec![];
    for i in 0..n {
        let v = env.get(i).copied().unwrap_or(0.0);
        if v < 0.05 * peak {
            continue;
        }
        let (a, b) = (i.saturating_sub(half), (i + half + 1).min(n));
        let local = env.get(a..b).map_or(0.0, |w| w.iter().sum::<f64>() / w.len().max(1) as f64);
        if v <= local + margin {
            continue;
        }
        let (a, b) = (i.saturating_sub(radius), (i + radius + 1).min(n));
        let is_max = env.get(a..b).is_some_and(|w| w.iter().all(|x| *x <= v));
        if !is_max {
            continue;
        }
        if out.last().is_some_and(|last| i - last < min_gap.max(1)) {
            continue;
        }
        out.push(i);
    }
    out
}

/// A beat grid at `period` hops through the strongest phase of the envelope.
pub(crate) fn beat_grid(env: &[f64], period: f64) -> Vec<f64> {
    if !(period.is_finite() && period >= 1.0) {
        return vec![];
    }
    let steps = period.ceil() as usize;
    let score = |phase: f64| {
        let mut t = phase;
        let mut s = 0.0;
        while (t as usize) < env.len() {
            s += env.get(t.round() as usize).copied().unwrap_or(0.0);
            t += period;
        }
        s
    };
    let phase = (0..steps).map(|p| p as f64).max_by(|a, b| score(*a).total_cmp(&score(*b))).unwrap_or(0.0);
    let mut out = vec![];
    let mut t = phase;
    while t < env.len() as f64 {
        out.push(t);
        t += period;
    }
    out
}

/// Straighten a grid on the hits it lines up with: a least-squares phase and period through the
/// onset nearest each grid beat (within a quarter beat), so a tempo read slightly off doesn't
/// drift over a song and the beats sit on the hits rather than on the envelope's step. Returns
/// the refitted grid over `len` hops and its period, or the grid unchanged when too few beats
/// match.
pub(crate) fn fit_grid(grid: &[f64], onsets: &[usize], period: f64, len: usize) -> (Vec<f64>, f64) {
    let pairs: Vec<(f64, f64)> = grid
        .iter()
        .enumerate()
        .filter_map(|(k, g)| {
            let near = onsets.iter().map(|o| *o as f64).min_by(|a, b| (a - g).abs().total_cmp(&(b - g).abs()))?;
            ((near - g).abs() <= period / 4.0).then_some((k as f64, near))
        })
        .collect();
    if pairs.len() < 4 {
        return (grid.to_vec(), period);
    }
    let n = pairs.len() as f64;
    let (mx, my) = (pairs.iter().map(|p| p.0).sum::<f64>() / n, pairs.iter().map(|p| p.1).sum::<f64>() / n);
    let sxx: f64 = pairs.iter().map(|(x, _)| (x - mx).powi(2)).sum();
    let sxy: f64 = pairs.iter().map(|(x, y)| (x - mx) * (y - my)).sum();
    let b = if sxx > 0.0 { sxy / sxx } else { f64::NAN };
    let a = my - b * mx;
    if !(a.is_finite() && b.is_finite()) || (b - period).abs() > period * 0.1 {
        return (grid.to_vec(), period);
    }
    // The fitted line, from its first beat at or after hop 0.
    let k0 = (-a / b).ceil();
    let mut out = vec![];
    let mut k = k0;
    while out.len() < 100_000 {
        let t = a + b * k;
        if t >= len as f64 {
            break;
        }
        if t >= 0.0 {
            out.push(t);
        }
        k += 1.0;
    }
    (out, b)
}

/// Mono samples of a layer's sound at the analysis rate over its visible span, and the layer
/// time where they start; or the comp's mix over its work area (layer `None`).
fn analysis_audio(s: &Session, cid: ItemId, layer: Option<effectcraft_project::LayerId>) -> Result<(Vec<f32>, Tick)> {
    const C: &str = "audio.detectBeats";
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let (source, from, to): (Option<(ItemId, bool)>, Tick, Tick) = match layer {
        Some(lid) => {
            let l = comp.layer(lid).ok_or_else(|| bad(C, "no such layer"))?;
            let source = match &l.source {
                LayerSource::Footage { item } => match s.project.item(*item).map(|i| &i.kind) {
                    Some(ItemKind::Footage(f)) if f.has_audio => (*item, true),
                    _ => return Err(bad(C, "this layer has no sound; pick the music layer")),
                },
                LayerSource::Comp { item } => (*item, false),
                _ => return Err(bad(C, "pick a layer with sound (footage or a precomp)")),
            };
            (Some(source), l.layer_time(l.in_point).max(Tick::ZERO), l.layer_time(l.out_point))
        }
        None => (None, comp.work_area.0, comp.work_area.1),
    };
    let span = (to - from).seconds().min(MAX_SECONDS);
    if span.is_nan() || span <= 0.0 {
        return Err(bad(C, "nothing to analyse: the layer or work area is empty"));
    }
    let total = (span * RATE as f64) as usize;
    let mut mono = Vec::with_capacity(total);
    let chunk = RATE as usize;
    let mut done = 0usize;
    while done < total {
        let n = chunk.min(total - done);
        let t = from + Tick::from_seconds_f64(done as f64 / RATE as f64);
        let buf = match source {
            Some((item, true)) => match s.project.item(item).map(|i| &i.kind) {
                Some(ItemKind::Footage(f)) => s.footage.audio(item, f, t, n, RATE),
                _ => None,
            },
            Some((item, false)) => Some(effectcraft_render::audio::mix_comp(&s.project, s.footage.as_ref(), s.expr.as_deref(), item, t, n, RATE)),
            None => Some(effectcraft_render::audio::mix_comp(&s.project, s.footage.as_ref(), s.expr.as_deref(), cid, t, n, RATE)),
        }
        .unwrap_or_else(|| vec![0.0; n * 2]);
        mono.extend(buf.as_chunks::<2>().0.iter().map(|c| 0.5 * (c[0] + c[1])));
        done += n;
    }
    Ok((mono, from))
}

/// `audio.detectBeats`: find the beats of a music layer (or the comp's mix) and mark them, so
/// cuts can be placed on the beat. `mode: onsets` marks the hits it hears; `grid` (default)
/// marks a steady beat at the detected tempo, phase-locked to the strongest hits.
fn detect_beats(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "audio.detectBeats";
    let (cid, layer) = if p.get("layer").is_some() {
        let (c, l) = layer_p(s, p, C)?;
        (c, Some(l))
    } else {
        (comp_id(s, p)?, None)
    };
    let mode = str_p(p, "mode").unwrap_or("grid");
    if !matches!(mode, "grid" | "onsets") {
        return Err(bad(C, format!("mode: grid|onsets, not `{mode}`")));
    }
    let sensitivity = f_p(p, "sensitivity").filter(|v| v.is_finite()).unwrap_or(0.5).clamp(0.0, 1.0);
    let min_gap_s = f_p(p, "minGap").filter(|v| v.is_finite() && *v > 0.0).unwrap_or(0.25).clamp(0.05, 10.0);
    let every = f_p(p, "every").filter(|v| v.is_finite() && *v >= 1.0).map_or(1, |v| v.min(64.0) as usize);
    let add_markers = b_p(p, "markers").unwrap_or(true);
    let comment = str_p(p, "comment").unwrap_or("beat").to_string();

    let (mono, from) = analysis_audio(s, cid, layer)?;
    let env = onset_envelope(&mono);
    let mut tempo = estimate_tempo(&env);
    let hop_s = HOP as f64 / RATE as f64;
    let onsets = pick_onsets(&env, (min_gap_s / hop_s).round() as usize, sensitivity);
    let hops: Vec<f64> = match (mode, tempo) {
        ("grid", Some((_, period, confidence))) => {
            let (grid, fitted) = fit_grid(&beat_grid(&env, period), &onsets, period, env.len());
            tempo = Some((bpm_of(fitted), fitted, confidence));
            grid
        }
        _ => onsets.iter().map(|i| *i as f64).collect(),
    };
    // Layer time (for a layer) or comp time (for the comp) of each beat, every `every`th.
    let times: Vec<Tick> = hops.iter().step_by(every).map(|h| from + Tick::from_seconds_f64(h * hop_s)).collect();
    let comp_times: Vec<f64> = match layer {
        Some(lid) => {
            let l = s.project.comp(cid).and_then(|c| c.layer(lid)).ok_or_else(|| bad(C, "no such layer"))?;
            times.iter().map(|t| l.comp_time(*t).seconds()).collect()
        }
        None => times.iter().map(|t| t.seconds()).collect(),
    };
    if add_markers && !times.is_empty() {
        s.edit("Detect Beats", None, |proj, _| {
            let mk = |t: Tick| Marker { time: t, comment: comment.clone(), ..Default::default() };
            let markers = match layer {
                Some(lid) => &mut layer_mut(proj, cid, lid)?.markers,
                None => &mut proj.comp_mut(cid).ok_or(EngineError::NoComp)?.markers,
            };
            // Re-running replaces the earlier beat markers instead of piling up.
            markers.retain(|m| m.comment != comment);
            markers.extend(times.iter().map(|t| mk(*t)));
            markers.sort_by_key(|m| m.time);
            Ok(())
        })?;
    }
    Ok(json!({
        "bpm": tempo.map(|(b, _, _)| (b * 10.0).round() / 10.0),
        "confidence": tempo.map(|(_, _, c)| (c * 100.0).round() / 100.0),
        "mode": if mode == "grid" && tempo.is_none() { "onsets" } else { mode },
        "count": comp_times.len(),
        "times": comp_times.iter().map(|t| (t * 1000.0).round() / 1000.0).collect::<Vec<_>>(),
        "markers": if add_markers { if layer.is_some() { "layer" } else { "comp" } } else { "none" },
    }))
}

// ---------------------------------------------------------------- layer.cutToBeats

/// `layer.cutToBeats`: lay the clips end to end so every cut lands on a beat marker of the
/// music layer (run `audio.detectBeats` on it first). Clip `i` starts on beat `start + i×every`
/// with its first visible frame and is trimmed to end `every` beats later; a clip whose content
/// runs out first ends early and is listed in `short`, clips beyond the last beat in
/// `unplaced`. One undo step.
fn cut_to_beats(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "layer.cutToBeats";
    let (cid, clips) = layers_p(s, p)?;
    if clips.is_empty() {
        return Err(bad(C, "give the clips as `layers`, in order"));
    }
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let music_v = p.get("music").ok_or_else(|| bad(C, "`music`: the layer with the beat markers"))?;
    let music = resolve_layer(comp, music_v).ok_or_else(|| bad(C, format!("no layer {music_v}")))?;
    if clips.contains(&music) {
        return Err(bad(C, "the music layer can't also be a clip"));
    }
    let comment = str_p(p, "comment").unwrap_or("beat");
    let every = f_p(p, "every").filter(|v| v.is_finite() && *v >= 1.0).map_or(1, |v| v.min(64.0) as usize);
    let first = f_p(p, "start").filter(|v| v.is_finite() && *v >= 0.0).map_or(0, |v| v.min(100_000.0) as usize);
    let fr = comp.frame_rate;
    let ml = comp.layer(music).ok_or_else(|| bad(C, "no such music layer"))?;
    let mut beats: Vec<Tick> = ml
        .markers
        .iter()
        .filter(|m| m.comment == comment)
        .map(|m| fr.snap_nearest(ml.comp_time(m.time)))
        .filter(|t| *t >= ml.in_point && *t <= ml.out_point)
        .collect();
    beats.sort();
    beats.dedup();
    if beats.len() < first.saturating_add(every).saturating_add(1) {
        return Err(bad(C, format!("the music layer has {} `{comment}` markers; run audio.detectBeats on it first", beats.len())));
    }
    // Where each clip's content ends, in comp time (None: no end, e.g. solids and text).
    let content_end = |l: &effectcraft_project::Layer| -> Option<Tick> {
        let dur = match &l.source {
            LayerSource::Footage { item } => match s.project.item(*item).map(|i| &i.kind) {
                Some(ItemKind::Footage(f)) if f.kind != effectcraft_project::FootageKind::Still => Some(f.looped_duration()),
                _ => None,
            },
            LayerSource::Comp { item } => s.project.comp(*item).map(|c| c.duration),
            _ => None,
        }?;
        Some(l.comp_time(dur))
    };
    let mut plan = vec![];
    let mut unplaced = vec![];
    for (i, lid) in clips.iter().enumerate() {
        let k = first.saturating_add(i.saturating_mul(every));
        match (beats.get(k), beats.get(k.saturating_add(every)), comp.layer(*lid)) {
            (Some(a), Some(b), Some(l)) => plan.push((*lid, *a, *b, content_end(l))),
            _ => unplaced.push(lid.0),
        }
    }
    let one_frame = fr.tick_of(1);
    let mut placed = vec![];
    let mut short = vec![];
    s.edit("Cut to Beats", None, |proj, _| {
        for (lid, a, b, end) in &plan {
            let l = layer_mut(proj, cid, *lid)?;
            let shift = *a - l.in_point;
            l.start_time = l.start_time + shift;
            l.in_point = *a;
            let out = match end {
                Some(e) if *e + shift < *b => {
                    short.push(lid.0);
                    *e + shift
                }
                _ => *b,
            };
            l.out_point = out.max(*a + one_frame);
            placed.push(json!({"layer": lid.0, "in": a.seconds(), "out": l.out_point.seconds()}));
        }
        Ok(())
    })?;
    Ok(json!({"placed": placed, "short": short, "unplaced": unplaced, "beats": beats.len()}))
}

// ---------------------------------------------------------------- text.substituteFonts

/// Whether `font` matches `pattern`: the same name in any case, or a prefix ending in `*`.
fn font_matches(pattern: &str, font: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => font.to_ascii_lowercase().starts_with(&prefix.to_ascii_lowercase()),
        None => font.eq_ignore_ascii_case(pattern),
    }
}

/// `text.substituteFonts`: draw text set in fonts this machine doesn't have with stand-ins,
/// across the whole project, keeping the original font names (After Effects' missing-font
/// substitution): installing the real font and clearing the stand-in brings it back. `map`
/// pairs a font (PostScript or family name, or a prefix ending in `*`) with its stand-in, or
/// with `null` to clear. Installed fonts are left alone unless `all`. One undo step.
fn substitute_fonts(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "text.substituteFonts";
    let map: Vec<(String, Option<String>)> = p
        .get("map")
        .and_then(Value::as_object)
        .ok_or_else(|| bad(C, "`map`: {font name or prefix*: stand-in font, or null to clear}"))?
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str().filter(|t| !t.is_empty()).map(str::to_string)))
        .collect();
    if map.is_empty() {
        return Err(bad(C, "`map` is empty"));
    }
    // A stand-in that isn't installed would fall back again.
    if let Some(t) = map.iter().filter_map(|(_, to)| to.as_deref()).find(|t| crate::text::fonts::resolve(t, "").missing) {
        return Err(bad(C, format!("stand-in font `{t}` is not installed (text.fonts lists the installed ones)")));
    }
    let all = b_p(p, "all").unwrap_or(false);
    // The stand-in for one style run: Some(new render font) when it changes.
    let pick = |font: &str, style: &str, current: &Option<String>| -> Option<Option<String>> {
        let to = map.iter().find(|(pat, _)| font_matches(pat, font)).map(|(_, to)| to.clone())?;
        if to.is_some() && !all && !crate::text::fonts::resolve(font, style).missing {
            return None;
        }
        (to != *current).then_some(to)
    };
    let (runs, layers) = s.edit("Substitute Fonts", None, |proj, _| {
        let (mut runs, mut layers) = (0usize, 0usize);
        let ids: Vec<ItemId> = proj.comps().map(|(id, _)| *id).collect();
        for cid in ids {
            let Some(c) = proj.comp_mut(cid) else { continue };
            for l in &mut c.layers {
                let Some(pr) = l.props.prop_mut("text/sourceText") else { continue };
                let mut changed = 0usize;
                let mut fix = |doc: &mut effectcraft_keyframe::TextDoc| {
                    if let Some(to) = pick(&doc.font, &doc.style, &doc.render_font) {
                        doc.render_font = to;
                        changed += 1;
                    }
                    for run in &mut doc.runs {
                        let st = &mut run.style;
                        if let Some(to) = pick(&st.font, &st.style, &st.render_font) {
                            st.render_font = to;
                            changed += 1;
                        }
                    }
                };
                if let effectcraft_keyframe::Value::Text(doc) = &mut pr.value {
                    fix(doc);
                }
                for k in &mut pr.keys {
                    if let effectcraft_keyframe::Value::Text(doc) = &mut k.value {
                        fix(doc);
                    }
                }
                if changed > 0 {
                    runs += changed;
                    layers += 1;
                }
            }
        }
        Ok((runs, layers))
    })?;
    Ok(json!({"runs": runs, "layers": layers}))
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            "comp.reframe",
            "Reframe Composition",
            [],
            None,
            "{comp? (default active), aspect?: 9:16|16:9|1:1|4:5 | width + height, mode?: blur (default: fitted over a blurred fill)|fit|fill, blur? (60), dim? (brightness of the blurred copy, -40), name?} → {comp, front, background, scale}",
            has_comp,
            reframe
        ),
        cmd!(
            "audio.detectBeats",
            "Detect Beats",
            [],
            None,
            "{layer? (music layer; default the comp's mix), comp?, mode?: grid (default: steady beats at the detected tempo)|onsets (the hits heard), sensitivity? 0–1 (onsets, 0.5), minGap? s (onsets, 0.25), every? n (keep every nth beat), markers?: bool (default true; layer markers, or comp markers without a layer), comment? (\"beat\"; re-running replaces markers with it)} → {bpm, confidence, count, times (comp s)}",
            has_comp,
            detect_beats
        ),
        cmd!(
            "layer.cutToBeats",
            "Cut to Beats",
            [],
            None,
            "{layers (the clips, in order), music (layer with beat markers from audio.detectBeats), every? (beats per clip, 1), start? (first beat index, 0), comment? (\"beat\"), comp?} → {placed: [{layer, in, out}], short (clips that ran out early), unplaced (no beats left)}",
            has_comp,
            cut_to_beats
        ),
        cmd!(
            "text.substituteFonts",
            "Substitute Fonts",
            [],
            None,
            "{map: {font (PostScript or family name, or prefix*): stand-in font, or null to clear}, all?: bool (also substitute installed fonts)} → {runs, layers}; the original font names stay (file.findMissing {what: fonts} shows them as substituted)",
            always,
            substitute_fonts
        ),
    ]
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// `secs` of mono audio at the analysis rate with a short decaying 80 Hz thump (a kick) every
    /// `period` seconds from `start`, over quiet noise.
    fn clicks(secs: f64, period: f64, start: f64) -> Vec<f32> {
        let n = (secs * RATE as f64) as usize;
        let mut seed = 7u32;
        let mut out: Vec<f32> = (0..n)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (seed >> 8) as f32 / (1u32 << 24) as f32 * 0.01 - 0.005
            })
            .collect();
        let mut t = start;
        while t < secs {
            let i0 = (t * RATE as f64) as usize;
            for k in 0..(RATE as usize / 10) {
                let env = (-(k as f32) / (RATE as f32 * 0.03)).exp();
                if let Some(x) = out.get_mut(i0 + k) {
                    *x += 0.8 * env * (2.0 * std::f32::consts::PI * 80.0 * k as f32 / RATE as f32).sin();
                }
            }
            t += period;
        }
        out
    }

    #[test]
    fn finds_the_tempo_and_the_hits_of_a_click_track() {
        let env = onset_envelope(&clicks(12.0, 0.5, 0.3));
        let (bpm, period, confidence) = estimate_tempo(&env).unwrap();
        assert!((bpm - 120.0).abs() < 2.0, "bpm {bpm}");
        assert!(confidence > 0.3, "confidence {confidence}");
        let hop = HOP as f64 / RATE as f64;
        let hits = pick_onsets(&env, (0.25 / hop) as usize, 0.5);
        assert!((22..=24).contains(&hits.len()), "{} hits", hits.len());
        for w in hits.windows(2) {
            let gap = (w[1] - w[0]) as f64 * hop;
            assert!((gap - 0.5).abs() < 0.05, "gap {gap}");
        }
        let first = hits[0] as f64 * hop;
        assert!((first - 0.3).abs() < 0.05, "first hit {first}");
        // The grid is steady at the tempo and sits on the hits.
        let grid = beat_grid(&env, period);
        assert!((22..=25).contains(&grid.len()), "{} beats", grid.len());
        let near = grid.iter().filter(|g| hits.iter().any(|h| (**g - *h as f64).abs() * hop < 0.05)).count();
        assert!(near >= 20, "{near} grid beats on hits");
    }

    #[test]
    fn the_grid_is_refitted_on_the_hits_so_it_does_not_drift() {
        let env = onset_envelope(&clicks(30.0, 0.5, 0.3));
        let hop = HOP as f64 / RATE as f64;
        let hits = pick_onsets(&env, (0.25 / hop) as usize, 0.5);
        // A period 1% short drifts a whole hit-width over 30 s; the fit pulls it back.
        let off = 0.5 / hop * 0.99;
        let (grid, period) = fit_grid(&beat_grid(&env, off), &hits, off, env.len());
        assert!((bpm_of(period) - 120.0).abs() < 0.3, "bpm {}", bpm_of(period));
        assert!((58..=60).contains(&grid.len()), "{} beats", grid.len());
        for g in &grid {
            let t = g * hop;
            let k = ((t - 0.3) / 0.5).round();
            assert!((t - (0.3 + 0.5 * k)).abs() < 0.025, "beat at {t:.3} s");
        }
        // Too few matching hits: the grid is left as it was.
        let (same, p) = fit_grid(&[1.0, 2.0], &[], 21.5, 100);
        assert_eq!((same, p), (vec![1.0, 2.0], 21.5));
    }

    #[test]
    fn a_faster_track_reads_faster_and_silence_has_no_tempo() {
        let env = onset_envelope(&clicks(12.0, 60.0 / 150.0, 0.1));
        let (bpm, _, _) = estimate_tempo(&env).unwrap();
        assert!((bpm - 150.0).abs() < 3.0, "bpm {bpm}");
        assert!(estimate_tempo(&onset_envelope(&vec![0.0; RATE as usize * 3])).is_none());
        assert!(pick_onsets(&[], 10, 0.5).is_empty());
        assert!(beat_grid(&[0.0; 10], f64::NAN).is_empty());
    }

    #[test]
    fn reframe_makes_a_vertical_version_in_one_undo_step() {
        let mut s = Session::default();
        let src =
            s.execute("comp.new", json!({"name": "Wide", "width": 1920, "height": 1080, "frameRate": 30, "duration": 3})).unwrap()["comp"].as_u64().unwrap();
        let r = s.execute("comp.reframe", json!({"comp": src, "aspect": "9:16"})).unwrap();
        let new = ItemId(r["comp"].as_u64().unwrap());
        let c = s.project.comp(new).unwrap();
        assert_eq!((c.width, c.height), (1080, 1920));
        assert_eq!(c.duration, Tick::from_seconds_f64(3.0));
        assert_eq!(c.layers.len(), 2, "sharp copy over a blurred one");
        let scale = |i: usize| c.layers[i].props.prop("transform/scale").unwrap().value.clone();
        let fit = 1080.0 / 1920.0 * 100.0;
        let fill = 1920.0 / 1080.0 * 100.0;
        assert_eq!(scale(0), effectcraft_keyframe::Value::Vec3([fit, fit, 100.0]));
        assert_eq!(scale(1), effectcraft_keyframe::Value::Vec3([fill, fill, 100.0]));
        assert_eq!(c.layers[1].effects().map_or(0, |fx| fx.groups().count()), 2, "blur and darken");
        assert_eq!(s.project.item(new).unwrap().name, "Wide 1080x1920");
        // One undo removes the whole reframe.
        s.execute("edit.undo", json!({})).unwrap();
        assert!(s.project.comp(new).is_none());
        // Fill and fit modes: one layer; explicit sizes; bad input is an error.
        let r = s.execute("comp.reframe", json!({"comp": src, "width": 1080, "height": 1080, "mode": "fill", "name": "Sq"})).unwrap();
        let c = s.project.comp(ItemId(r["comp"].as_u64().unwrap())).unwrap();
        assert_eq!(c.layers.len(), 1);
        assert_eq!(c.layers[0].props.prop("transform/scale").unwrap().value, effectcraft_keyframe::Value::Vec3([100.0, 100.0, 100.0]));
        assert!(s.execute("comp.reframe", json!({"comp": src, "aspect": "3:7"})).is_err());
        assert!(s.execute("comp.reframe", json!({"comp": src, "aspect": "9:16", "mode": "zoom"})).is_err());
        assert!(s.execute("comp.reframe", json!({"comp": src, "width": 0, "height": 100})).is_err());
    }

    #[test]
    fn cut_to_beats_puts_each_cut_on_a_beat_and_reports_short_clips() {
        let mut s = Session::default();
        let clip = |s: &mut Session, name: &str, secs: f64| {
            s.execute("comp.new", json!({"name": name, "width": 64, "height": 64, "frameRate": 30, "duration": secs})).unwrap()["comp"].as_u64().unwrap()
        };
        let (a, b, c) = (clip(&mut s, "A", 3.0), clip(&mut s, "B", 3.0), clip(&mut s, "Short", 1.0));
        s.execute("comp.new", json!({"name": "Main", "width": 64, "height": 64, "frameRate": 30, "duration": 10})).unwrap();
        let music = s.execute("layer.newSolid", json!({"name": "Music"})).unwrap()["layer"].as_u64().unwrap();
        // Beats every 0.75 s, the first at 0.5 s.
        for i in 0..10 {
            s.execute("layer.addMarker", json!({"layers": [music], "time": 0.5 + 0.75 * i as f64, "comment": "beat"})).unwrap();
        }
        let layer = |s: &mut Session, item: u64| s.execute("layer.addItem", json!({"item": item, "time": 0})).unwrap()["layer"].as_u64().unwrap();
        let (la, lb, lc) = (layer(&mut s, a), layer(&mut s, b), layer(&mut s, c));
        // A clip trimmed at its head keeps that first visible frame on its beat.
        s.execute("layer.timing", json!({"layers": [la], "in": 0.5})).unwrap();
        let r = s.execute("layer.cutToBeats", json!({"layers": [la, lb, lc], "music": music, "every": 2})).unwrap();
        let span = |s: &Session, id: u64| {
            let l = s.active_comp().unwrap().layers.iter().find(|l| l.id.0 == id).unwrap().clone();
            (l.in_point.seconds(), l.out_point.seconds(), l.layer_time(l.in_point).seconds())
        };
        assert_eq!(span(&s, la), (0.5, 2.0, 0.5), "beats 0 to 2, from its trimmed first frame");
        assert_eq!(span(&s, lb), (2.0, 3.5, 0.0), "beats 2 to 4");
        assert_eq!(span(&s, lc), (3.5, 4.5, 0.0), "the 1 s clip ends with its content, before beat 6 (5.0 s)");
        assert_eq!(r["short"], json!([lc]));
        assert_eq!(r["unplaced"], json!([]));
        // One undo step; too few beats or a missing music layer are errors.
        s.execute("edit.undo", json!({})).unwrap();
        assert_eq!(span(&s, lb).0, 0.0);
        assert!(s.execute("layer.cutToBeats", json!({"layers": [la, lb, lc], "music": music, "every": 2, "start": 8})).is_err());
        assert!(s.execute("layer.cutToBeats", json!({"layers": [la], "music": 999})).is_err());
        assert!(s.execute("layer.cutToBeats", json!({"layers": [music], "music": music})).is_err());
    }

    #[test]
    fn missing_fonts_get_stand_ins_that_keep_the_original_name() {
        let mut s = Session::default();
        s.execute("comp.new", json!({"name": "Main", "width": 200, "height": 100, "frameRate": 30, "duration": 2})).unwrap();
        let a = s.execute("layer.newText", json!({"text": "Title"})).unwrap()["layer"].as_u64().unwrap();
        s.execute("layer.setText", json!({"layer": a, "font": "TrajanPro3-Light"})).unwrap();
        let b = s.execute("layer.newText", json!({"text": "Kept"})).unwrap()["layer"].as_u64().unwrap();
        s.execute("layer.setText", json!({"layer": b, "font": "Inter", "style": "Bold"})).unwrap();
        let doc = |s: &Session, id: u64| {
            let l = s.active_comp().unwrap().layers.iter().find(|l| l.id.0 == id).unwrap().clone();
            l.props.prop("text/sourceText").unwrap().value.as_text().unwrap().clone()
        };
        let r = s.execute("text.substituteFonts", json!({"map": {"TrajanPro3-*": "Inter", "Inter": "JetBrains Mono"}})).unwrap();
        assert_eq!(r["layers"], 1, "the installed Inter is left alone");
        assert!(r["runs"].as_u64().unwrap() >= 1);
        let d = doc(&s, a);
        assert_eq!((d.font.as_str(), d.render_font.as_deref()), ("TrajanPro3-Light", Some("Inter")));
        assert_eq!(doc(&s, b).render_font, None);
        // `all` substitutes installed fonts too; null clears; one undo step each.
        s.execute("text.substituteFonts", json!({"map": {"Inter": "JetBrains Mono"}, "all": true})).unwrap();
        assert_eq!(doc(&s, b).render_font.as_deref(), Some("JetBrains Mono"));
        s.execute("edit.undo", json!({})).unwrap();
        assert_eq!(doc(&s, b).render_font, None);
        s.execute("text.substituteFonts", json!({"map": {"TrajanPro3-Light": null}})).unwrap();
        assert_eq!(doc(&s, a).render_font, None);
        assert!(s.execute("text.substituteFonts", json!({"map": {"TrajanPro3-*": "NoSuchFont-Regular"}})).is_err(), "stand-ins must be installed");
        assert!(s.execute("text.substituteFonts", json!({"map": {}})).is_err());
    }

    #[test]
    fn detect_beats_refuses_layers_without_sound() {
        let mut s = Session::default();
        s.execute("comp.new", json!({"name": "Main", "width": 64, "height": 64, "frameRate": 30, "duration": 2})).unwrap();
        let solid = s.execute("layer.newSolid", json!({})).unwrap()["layer"].as_u64().unwrap();
        assert!(s.execute("audio.detectBeats", json!({"layer": solid})).is_err());
        assert!(s.execute("audio.detectBeats", json!({"mode": "loud"})).is_err());
        // A silent comp: no tempo, no markers, no panic.
        let r = s.execute("audio.detectBeats", json!({})).unwrap();
        assert_eq!(r["count"], 0);
        assert!(r["bpm"].is_null());
    }
}
