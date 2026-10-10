//! Copy and Paste with a single channel targeted in the Channels panel (Photoshop, #2680, #2307).
//!
//! Copy (and Copy Merged) with a colour channel targeted puts that channel on the clipboard as a
//! grayscale image, in the document's bit depth; with an alpha channel targeted, that channel.
//! Paste with a colour channel targeted writes the clipboard's luminosity into that channel of
//! the active layer instead of making a new layer. Pasting into an alpha channel, the Quick Mask
//! or a layer mask is [`super::paste_to_target`].

use photocraft_algo::selection as sel;
use photocraft_color::{ColorMode, PixelFormat};
use photocraft_doc::Layer;
use photocraft_geom::Rect;
use photocraft_raster::Surface;
use serde_json::{Value, json};

use super::{Clip, lift, merged_surface, paste_offset, shifted};
use crate::channel_cmds::ChannelTarget;
use crate::{DocState, EngineError, Result, Session};

/// Copy the targeted channel, if a single colour or alpha channel is targeted (else None: the
/// caller copies the layer or the merged image as usual).
pub(super) fn copy(s: &mut Session, merged: bool) -> Option<Result<Value>> {
    let clip = targeted_clip(s.active()?, merged)?;
    Some(clip.and_then(|clip| {
        if clip.bounds.is_empty() {
            return Err(EngineError::Other("Could not copy: the selected area is empty".into()));
        }
        let b = clip.bounds;
        s.clipboard = Some(clip);
        Ok(json!({"bounds": [b.x0, b.y0, b.width(), b.height()]}))
    }))
}

fn targeted_clip(d: &DocState, merged: bool) -> Option<Result<Clip>> {
    let canvas = d.doc.bounds();
    let selection = d.doc.selection.as_ref();
    // A channel has no transparency of its own: without a selection it copies the whole canvas.
    let area = selection.map_or(canvas, |m| m.content_bounds().intersect(&canvas));
    let plane = match d.channel_view.target {
        ChannelTarget::Composite => return None,
        ChannelTarget::Color(k) if merged => plane(&merged_surface(&d.doc), k, area),
        ChannelTarget::Color(k) => match d.active_layer.and_then(|id| d.doc.layer(id)).and_then(Layer::surface) {
            Some(surf) => plane(surf, k, area),
            None => Err(EngineError::Other("the active layer has no pixels".into())),
        },
        ChannelTarget::Alpha(i) => match d.doc.channels.get(i) {
            Some(c) => plane(&c.surface, 0, area),
            None => Err(EngineError::Other(format!("no alpha channel {i}"))),
        },
    };
    Some(plane.map(|p| lift(&p, selection, canvas)))
}

/// Channel `k` of `src` across `area` as a grayscale surface (with `src`'s transparency, if any)
/// at `src`'s bit depth. The values are the channel's own, as Split Channels shows them.
fn plane(src: &Surface, k: usize, area: Rect) -> Result<Surface> {
    let fmt = src.format();
    let colors = fmt.mode.color_channels();
    if k >= colors {
        return Err(EngineError::Other(format!("no colour channel {k} (the image has {colors})")));
    }
    let gray = PixelFormat::new(ColorMode::Grayscale, fmt.sample, true);
    let mut out = Surface::new(gray);
    if area.is_empty() {
        return Ok(out);
    }
    let n = fmt.channels();
    let px = src.read_region(area);
    let mut v = Vec::with_capacity(px.len() / n.max(1) * 2);
    for p in px.chunks_exact(n.max(1)) {
        let a = if fmt.alpha { p.get(n - 1).copied().unwrap_or(1.0) } else { 1.0 };
        let value = if a > 0.0 { p.get(k).copied().unwrap_or(0.0) } else { 0.0 };
        v.extend_from_slice(&[value, a]);
    }
    out.write_region(area, &v);
    out.prune();
    Ok(out)
}

/// Paste with colour channel `k` of the active layer targeted: the clipboard's luminosity,
/// placed as a paste places it, over that channel (weighted by the clipboard's transparency and
/// `limit`, for Paste Into / Outside). The layer's other channels and its transparency stay as
/// they were, and no layer is made. One history step.
pub(crate) fn paste_into_color(s: &mut Session, p: &Value, in_place: bool, limit: Option<&Surface>, label: &str, k: usize) -> Result<Value> {
    let clip = s.clipboard.clone().ok_or(EngineError::Other("the clipboard is empty".into()))?;
    let d = s.active().ok_or(EngineError::NoDocument)?;
    let canvas = d.doc.bounds();
    let id = d.active_layer.ok_or(EngineError::Other("no active layer".into()))?;
    let (dx, dy) = paste_offset(&clip, canvas, p, in_place);
    let src = shifted(&clip.surface, dx, dy);
    let gray = PixelFormat::new(ColorMode::Grayscale, src.format().sample, true);
    let src = if src.format() == gray { src } else { src.convert(gray) };
    let area = src.content_bounds().intersect(&canvas);
    s.edit(label, |doc, _| {
        let surf = crate::commands::paint_surface(doc, id, &Value::Null)?;
        let fmt = surf.format();
        let colors = fmt.mode.color_channels();
        if k >= colors {
            return Err(EngineError::Other(format!("no colour channel {k} (the layer has {colors})")));
        }
        if !area.is_empty() {
            let n = fmt.channels();
            let g = src.read_region(area);
            let weight = limit.map(|m| sel::mask_from_surface(Some(m), area));
            let mut px = surf.read_region(area);
            for (i, (dst, s)) in px.chunks_exact_mut(n.max(1)).zip(g.as_chunks::<2>().0).enumerate() {
                let a = s[1] * weight.as_ref().map_or(1.0, |w| w.get(i).copied().unwrap_or(0.0));
                if let Some(c) = dst.get_mut(k) {
                    *c += (s[0] - *c) * a;
                }
            }
            surf.write_region(area, &px);
            surf.prune();
        }
        doc.selection = None;
        Ok(())
    })?;
    Ok(json!({"layer": id.0, "channel": k, "offset": [dx, dy]}))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 40×40 RGB document whose Background has distinct red, green and blue values.
    fn rgb_session(depth: u32) -> Session {
        let mut s = Session::new();
        s.execute("file.new", json!({"width": 40, "height": 40, "depth": depth})).unwrap();
        s.edit("paint", |doc, a| {
            let surf = doc.layer_mut(a.unwrap()).unwrap().surface_mut().unwrap();
            let n = surf.format().channels();
            surf.fill_rect(Rect::new(0, 0, 40, 40), &[0.2, 0.5, 0.9, 1.0][..n]);
            surf.fill_rect(Rect::new(0, 0, 20, 40), &[0.8, 0.5, 0.1, 1.0][..n]);
            Ok(())
        })
        .unwrap();
        s
    }

    fn active_pixel(s: &Session, x: i32, y: i32) -> Vec<f32> {
        let st = s.active().unwrap();
        st.doc.layer(st.active_layer.unwrap()).unwrap().surface().unwrap().pixel(x, y)
    }

    /// #2680 / #2307: copy the Red channel, target Blue, paste: Blue now holds the former Red,
    /// Red and Green are unchanged, and no layer was added. One undo step restores it.
    #[test]
    fn copy_red_paste_into_blue() {
        for depth in [8, 16, 32] {
            let mut s = rgb_session(depth);
            let layers = s.active().unwrap().doc.layers.len();
            let before: Vec<_> = [(5, 5), (30, 5)].iter().map(|&(x, y)| active_pixel(&s, x, y)).collect();
            s.execute("channel.target", json!({"channel": "red"})).unwrap();
            s.execute("edit.copy", json!({})).unwrap();
            let clip = s.clipboard.as_ref().unwrap();
            assert_eq!(clip.surface.format().mode, ColorMode::Grayscale, "depth {depth}: the channel copies as grayscale");
            assert_eq!(clip.bounds, Rect::new(0, 0, 40, 40));
            s.execute("channel.target", json!({"channel": "blue"})).unwrap();
            s.execute("edit.paste", json!({})).unwrap();
            assert_eq!(s.active().unwrap().doc.layers.len(), layers, "depth {depth}: no new layer");
            for (i, &(x, y)) in [(5, 5), (30, 5)].iter().enumerate() {
                let (old, new) = (&before[i], active_pixel(&s, x, y));
                assert!((new[2] - old[0]).abs() < 0.01, "depth {depth} ({x},{y}): blue {} = former red {}", new[2], old[0]);
                assert!((new[0] - old[0]).abs() < 1e-6 && (new[1] - old[1]).abs() < 1e-6, "depth {depth}: red and green unchanged");
            }
            s.execute("edit.undo", json!({})).unwrap();
            assert_eq!(active_pixel(&s, 30, 5), before[1], "depth {depth}: one undo step");
        }
    }
}
