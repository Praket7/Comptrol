use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct DisplayGeometry {
    pub id: String,
    pub origin_logical: Point,
    pub size_logical: Point,
    pub scale: f64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct VirtualDesktop {
    pub revision: u64,
    pub displays: Vec<DisplayGeometry>,
}

impl VirtualDesktop {
    pub fn physical_to_virtual(&self, display_id: &str, point: Point) -> Option<Point> {
        let display = self
            .displays
            .iter()
            .find(|display| display.id == display_id)?;
        Some(Point {
            x: display.origin_logical.x + point.x / display.scale,
            y: display.origin_logical.y + point.y / display.scale,
        })
    }

    pub fn virtual_to_physical(&self, display_id: &str, point: Point) -> Option<Point> {
        let display = self
            .displays
            .iter()
            .find(|display| display.id == display_id)?;
        Some(Point {
            x: (point.x - display.origin_logical.x) * display.scale,
            y: (point.y - display.origin_logical.y) * display.scale,
        })
    }

    pub fn contains(&self, display_id: &str, point: Point) -> bool {
        let Some(display) = self
            .displays
            .iter()
            .find(|display| display.id == display_id)
        else {
            return false;
        };
        point.x >= display.origin_logical.x
            && point.y >= display.origin_logical.y
            && point.x < display.origin_logical.x + display.size_logical.x
            && point.y < display.origin_logical.y + display.size_logical.y
    }
}
