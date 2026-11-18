use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Left,
    Right,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Dock {
    pub side: Side,
    pub monitor: String,
    /// 把手顶端在工作区中的相对高度。拔插显示器或改变分辨率后仍可恢复。
    pub fraction: f64,
}

#[derive(Clone, Copy)]
pub struct Area {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Area {
    pub fn of(monitor: &tauri::Monitor) -> Self {
        let r = monitor.work_area();
        Self { x: r.position.x, y: r.position.y, width: r.size.width as i32, height: r.size.height as i32 }
    }

    pub fn snap_side(self, cursor_x: i32, scale: f64) -> Option<Side> {
        let threshold = (24.0 * scale).round() as i32;
        if cursor_x <= self.x + threshold {
            Some(Side::Left)
        } else if cursor_x >= self.x + self.width - threshold {
            Some(Side::Right)
        } else {
            None
        }
    }

    /// 返回窗口原点以及可点击区域在窗口内的横向偏移，均为物理像素。
    pub fn layout(self, dock: Option<&Dock>, anchor: (i32, i32), full: i32, width: i32, height: i32) -> (i32, i32, i32) {
        let (x, y, inset) = match dock {
            Some(dock) => {
                let y = self.y + (self.height as f64 * dock.fraction.clamp(0.0, 1.0)).round() as i32;
                match dock.side {
                    Side::Left => (self.x, y, 0),
                    Side::Right => (self.x + self.width - full, y, full - width),
                }
            }
            None => {
                let inset = (full - width) / 2;
                let left = (anchor.0 + inset).clamp(self.x, self.x + (self.width - width).max(0));
                (left - inset, anchor.1, inset)
            }
        };
        (x, y.clamp(self.y, self.y + (self.height - height).max(0)), inset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seam_and_bottom_corner_contain_the_handle_and_card() {
        let area = Area { x: -1920, y: -100, width: 1920, height: 1080 };
        for scale in [1.0, 1.25, 1.5] {
            for side in [Side::Left, Side::Right] {
                let dock = Dock { side, monitor: String::new(), fraction: 1.0 };
                for (w, h) in [(8, 42), (340, 440)] {
                    let (full, width, height) = ((372.0 * scale) as i32, (w as f64 * scale) as i32, (h as f64 * scale) as i32);
                    let (x, y, inset) = area.layout(Some(&dock), (0, 0), full, width, height);
                    let left = x + inset;
                    assert_eq!(left, if side == Side::Left { -1920 } else { -width });
                    assert_eq!(y + height, 980);
                }
            }
        }
        assert_eq!(area.snap_side(-2, 1.25), Some(Side::Right));
        assert_eq!(area.snap_side(-960, 1.25), None);
        assert_eq!(area.layout(None, (-2020, 950), 372, 340, 350), (-1936, 630, 16));
    }
}
