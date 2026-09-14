//! Minimal InfluxDB 1.x line-protocol builder.
//!
//! Only what the ingest service needs: string tags, integer fields, second
//! precision timestamps. Points without fields are dropped, because Influx
//! rejects them.

pub struct Point {
    measurement: String,
    tags: String,
    fields: String,
    ts: i64,
}

impl Point {
    pub fn new(measurement: &str, ts: i64) -> Self {
        Self {
            measurement: measurement.to_string(),
            tags: String::new(),
            fields: String::new(),
            ts,
        }
    }

    pub fn tag(&mut self, k: &str, v: &str) -> &mut Self {
        self.tags.push(',');
        self.tags.push_str(k);
        self.tags.push('=');
        self.tags.push_str(&escape_tag(v));
        self
    }

    /// Integer field; `None` is skipped entirely (sparse series are fine).
    pub fn ifield(&mut self, k: &str, v: Option<i64>) -> &mut Self {
        if let Some(v) = v {
            if !self.fields.is_empty() {
                self.fields.push(',');
            }
            self.fields.push_str(k);
            self.fields.push('=');
            self.fields.push_str(&v.to_string());
            self.fields.push('i'); // 1.8 integer literal
        }
        self
    }

    /// `None` when the point has no fields.
    pub fn finish(&self) -> Option<String> {
        if self.fields.is_empty() {
            return None;
        }
        Some(format!(
            "{}{} {} {}",
            self.measurement, self.tags, self.fields, self.ts
        ))
    }
}

fn escape_tag(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace(',', "\\,")
        .replace('=', "\\=")
        .replace(' ', "\\ ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_tags_fields_and_timestamp() {
        let mut p = Point::new("pebble_minute", 1_757_846_400);
        p.tag("device", "pt2-peter").tag("source", "alloy");
        p.ifield("steps", Some(12))
            .ifield("hr", None)
            .ifield("vmc", Some(340));
        assert_eq!(
            p.finish().unwrap(),
            "pebble_minute,device=pt2-peter,source=alloy steps=12i,vmc=340i 1757846400"
        );
    }

    #[test]
    fn point_without_fields_is_dropped() {
        let mut p = Point::new("pebble_minute", 1);
        p.tag("device", "x").ifield("steps", None);
        assert!(p.finish().is_none());
    }

    #[test]
    fn escapes_tag_values() {
        assert_eq!(escape_tag("a b,c=d\\e"), "a\\ b\\,c\\=d\\\\e");
    }
}
