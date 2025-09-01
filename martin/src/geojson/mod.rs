use std::collections::BTreeMap;
use std::fmt::{Debug, Formatter};
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use geozero::mvt::{Message as _, MvtWriter};
use geozero::{ColumnValue, FeatureProcessor, GeomProcessor, PropertyProcessor};
use martin_tile_utils::{Format, TileCoord, TileInfo};
use std::fs::File;
use tilejson::TileJSON;
use tilejson::tilejson;

use crate::file_config::FileError;
use crate::file_config::FileResult;
use crate::source::{TileData, TileInfoSource, UrlQuery};
use crate::{MartinResult, Source};

mod config;
//mod mvt;

pub use config::GeoJsonConfig;

#[derive(Clone)]
pub struct GeoJsonSource {
    id: String,
    path: PathBuf,
    geojson: Arc<geojson::GeoJson>,
    tile_options: geojson_vt_rs::TileOptions,
    tilejson: TileJSON,
    tile_info: TileInfo,
}

impl Debug for GeoJsonSource {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GeoJsonSource")
            .field("id", &self.id)
            .field("path", &self.path)
            .finish()
    }
}

impl GeoJsonSource {
    fn new(
        id: String,
        path: PathBuf,
        tile_options: geojson_vt_rs::TileOptions,
    ) -> FileResult<Self> {
        let tile_info = TileInfo::new(Format::Mvt, martin_tile_utils::Encoding::Uncompressed);
        let geojson_file =
            File::open(&path).map_err(|e: std::io::Error| FileError::IoError(e, path.clone()))?;

        // TODO: better error handling
        let geojson = geojson::GeoJson::from_reader(geojson_file)
            .map_err(|e| FileError::InvalidFilePath(path.clone()))?;

        let bounds = match &geojson {
            geojson::GeoJson::Geometry(geometry) => geojson_to_bounds(&geometry.value),
            geojson::GeoJson::Feature(feature) => match feature.geometry.as_ref() {
                Some(geom) => geojson_to_bounds(&geom.value),
                None => return Err(FileError::InvalidFilePath(path.clone())),
            },
            geojson::GeoJson::FeatureCollection(feature_collection) => {
                let mut bounds = tilejson::Bounds::new(f64::MAX, f64::MAX, f64::MIN, f64::MIN);
                for feature in &feature_collection.features {
                    match feature.geometry.as_ref() {
                        Some(geom) => {
                            let feat_bounds = geojson_to_bounds(&geom.value);
                            update_bounds(&mut bounds, feat_bounds.left, feat_bounds.bottom);
                            update_bounds(&mut bounds, feat_bounds.right, feat_bounds.top);
                        }
                        None => continue,
                    }
                }
                // no feature has geom so return an error
                if bounds == tilejson::Bounds::new(f64::MAX, f64::MAX, f64::MIN, f64::MIN) {
                    return Err(FileError::InvalidFilePath(path.clone()));
                }
                bounds
            }
        };

        let tilejson = tilejson! {
            tiles: vec![],
            vector_layers: geojson_to_vector_layer(&id, &geojson),
            bounds: bounds,
            minzoom: 0,
            maxzoom: 24, // from geojson-vt-rs max_zoom
        };

        return Ok(Self {
            id,
            path,
            geojson: Arc::new(geojson),
            tile_options,
            tilejson,
            tile_info,
        });
    }
}

#[async_trait]
impl Source for GeoJsonSource {
    fn get_id(&self) -> &str {
        &self.id
    }

    fn get_tilejson(&self) -> &TileJSON {
        &self.tilejson
    }

    fn get_tile_info(&self) -> TileInfo {
        self.tile_info
    }

    fn clone_source(&self) -> TileInfoSource {
        Box::new(self.clone())
    }

    fn benefits_from_concurrent_scraping(&self) -> bool {
        true
    }

    async fn get_tile(
        &self,
        xyz: TileCoord,
        _url_query: Option<&UrlQuery>,
    ) -> MartinResult<TileData> {
        // TODO: get from source (self)
        let tile = geojson_vt_rs::geojson_to_tile(
            &self.geojson,
            xyz.z,
            xyz.x,
            xyz.y,
            &self.tile_options,
            true,
            true,
        );

        let mut mvt_writer = MvtWriter::default();

        let _ = mvt_writer.dataset_begin(None);
        for (idx, feature) in tile.features.features.iter().enumerate() {
            let idx = idx as u64;
            let _ = mvt_writer.feature_begin(idx);
            let _ = mvt_writer.properties_begin();
            if let Some(properties) = feature.properties.as_ref() {
                for (key, json_value) in properties.iter() {
                    let stringified_json: String;
                    let value = match json_value {
                        serde_json::Value::Bool(bool) => ColumnValue::Bool(*bool),
                        serde_json::Value::Number(number) => number_to_columnvalue(number, false),
                        serde_json::Value::String(str) => ColumnValue::String(str),
                        json_value => {
                            stringified_json = json_value.to_string();
                            ColumnValue::Json(&stringified_json)
                        }
                    };
                    let _ = mvt_writer.property(idx as usize, key, &value);
                }
            }
            let _ = mvt_writer.properties_end();
            let _ = mvt_writer.geometry_begin();
            if let Some(geom) = feature.geometry.as_ref() {
                write_geojson_geom(&mut mvt_writer, &geom.value);
            }
            let _ = mvt_writer.geometry_end();
            let _ = mvt_writer.feature_end(idx);
        }
        let _ = mvt_writer.dataset_end();

        let mvt_tile = geozero::mvt::Tile {
            layers: vec![mvt_writer.layer(&self.id)],
        };
        Ok(mvt_tile.encode_to_vec())
    }
}

fn number_to_columnvalue(number: &serde_json::value::Number, prefer_f32: bool) -> ColumnValue {
    if number.is_u64() {
        let number_u64 = number.as_u64().unwrap();
        if u8::MIN as u64 <= number_u64 && number_u64 <= u8::MAX as u64 {
            ColumnValue::UByte(number_u64 as u8)
        } else if u16::MIN as u64 <= number_u64 && number_u64 <= u16::MAX as u64 {
            ColumnValue::UShort(number_u64 as u16)
        } else if u32::MIN as u64 <= number_u64 && number_u64 <= u32::MAX as u64 {
            ColumnValue::UInt(number_u64 as u32)
        } else {
            ColumnValue::ULong(number_u64)
        }
    } else if number.is_i64() {
        let number_i64 = number.as_i64().unwrap();
        if i8::MIN as i64 <= number_i64 && number_i64 <= i8::MAX as i64 {
            ColumnValue::Byte(number_i64 as i8)
        } else if i16::MIN as i64 <= number_i64 && number_i64 <= i16::MAX as i64 {
            ColumnValue::Short(number_i64 as i16)
        } else if i32::MIN as i64 <= number_i64 && number_i64 <= i32::MAX as i64 {
            ColumnValue::Int(number_i64 as i32)
        } else {
            ColumnValue::Long(number_i64)
        }
    } else {
        let number_f64 = number.as_f64().unwrap();
        if prefer_f32 {
            ColumnValue::Float(number_f64 as f32)
        } else {
            ColumnValue::Double(number_f64)
        }
    }
}

fn write_geojson_geom(mvt_writer: &mut MvtWriter, geom: &geojson::Value) {
    match geom {
        geojson::Value::Point(point) => {
            let x = point[0];
            let y = point[1];
            let _ = mvt_writer.point_begin(0);
            let _ = mvt_writer.xy(x, y, 0);
            let _ = mvt_writer.point_end(0);
        }
        geojson::Value::MultiPoint(points) => {
            let _ = mvt_writer.multipoint_begin(0, points.len());
            for point in points {
                let x = point[0];
                let y = point[1];
                let _ = mvt_writer.xy(x, y, 0);
            }
            let _ = mvt_writer.multipoint_end(0);
        }
        geojson::Value::LineString(points) => {
            let _ = mvt_writer.linestring_begin(true, points.len(), 0);
            for point in points {
                let x = point[0];
                let y = point[1];
                let _ = mvt_writer.xy(x, y, 0);
            }
            let _ = mvt_writer.linestring_end(true, 0);
        }
        geojson::Value::MultiLineString(linestrings) => {
            let _ = mvt_writer.multilinestring_begin(linestrings.len(), 0);
            for linestring in linestrings {
                let _ = mvt_writer.linestring_begin(false, linestring.len(), 0);
                for point in linestring {
                    let x = point[0];
                    let y = point[1];
                    let _ = mvt_writer.xy(x, y, 0);
                }
                let _ = mvt_writer.linestring_end(false, 0);
            }
            let _ = mvt_writer.multilinestring_end(0);
        }
        geojson::Value::Polygon(rings) => {
            let _ = mvt_writer.polygon_begin(true, rings.len(), 0);
            for ring in rings {
                let _ = mvt_writer.linestring_begin(false, ring.len(), 0);
                for point in ring {
                    let x = point[0];
                    let y = point[1];
                    let _ = mvt_writer.xy(x, y, 0);
                }
                let _ = mvt_writer.linestring_end(false, 0);
            }
            let _ = mvt_writer.polygon_end(true, 0);
        }
        geojson::Value::MultiPolygon(polygons) => {
            let _ = mvt_writer.multipolygon_begin(polygons.len(), 0);
            for polygon in polygons {
                let _ = mvt_writer.polygon_begin(false, polygon.len(), 0);
                for ring in polygon {
                    let _ = mvt_writer.linestring_begin(false, ring.len(), 0);
                    for point in ring {
                        let x = point[0];
                        let y = point[1];
                        let _ = mvt_writer.xy(x, y, 0);
                    }
                    let _ = mvt_writer.linestring_end(false, 0);
                }
                let _ = mvt_writer.polygon_end(false, 0);
            }
            let _ = mvt_writer.multipolygon_end(0);
        }
        _ => unimplemented!(),
    }
}

fn update_bounds(bounds: &mut tilejson::Bounds, x: f64, y: f64) {
    bounds.left = f64::min(bounds.left, x);
    bounds.right = f64::max(bounds.right, x);
    bounds.bottom = f64::min(bounds.bottom, y);
    bounds.top = f64::max(bounds.top, y);
}

fn geojson_to_bounds(geojson_value: &geojson::Value) -> tilejson::Bounds {
    let mut bounds = tilejson::Bounds::new(f64::MAX, f64::MAX, f64::MIN, f64::MIN);

    match geojson_value {
        geojson::Value::Point(point) => {
            update_bounds(&mut bounds, point[0], point[1]);
        }
        geojson::Value::MultiPoint(points) => {
            points.iter().for_each(|point| {
                update_bounds(&mut bounds, point[0], point[1]);
            });
        }
        geojson::Value::LineString(points) => {
            points.iter().for_each(|point| {
                update_bounds(&mut bounds, point[0], point[1]);
            });
        }
        geojson::Value::MultiLineString(linestrings) => {
            linestrings.iter().for_each(|linestring| {
                linestring.iter().for_each(|point| {
                    update_bounds(&mut bounds, point[0], point[1]);
                });
            });
        }
        geojson::Value::Polygon(rings) => {
            rings.iter().for_each(|ring| {
                ring.iter().for_each(|point| {
                    update_bounds(&mut bounds, point[0], point[1]);
                });
            });
        }
        geojson::Value::MultiPolygon(polygons) => {
            polygons.iter().for_each(|polygon| {
                polygon.iter().for_each(|ring| {
                    ring.iter().for_each(|point| {
                        update_bounds(&mut bounds, point[0], point[1]);
                    });
                });
            });
        }
        geojson::Value::GeometryCollection(geometries) => {
            for geometry in geometries {
                let col_bounds = geojson_to_bounds(&geometry.value);
                update_bounds(&mut bounds, col_bounds.left, col_bounds.bottom);
                update_bounds(&mut bounds, col_bounds.right, col_bounds.top);
            }
        }
    }

    return bounds;
}

// TODO: maybe break down fields subroutine into a function
fn geojson_to_vector_layer(
    layer_name: &str,
    geojson: &geojson::GeoJson,
) -> Vec<tilejson::VectorLayer> {
    let mut fields = BTreeMap::new();
    match geojson {
        geojson::GeoJson::Geometry(_geometry) => {
            vec![tilejson::VectorLayer::new(layer_name.to_string(), fields)]
        }
        geojson::GeoJson::Feature(feature) => {
            if let Some(properties) = feature.properties.as_ref() {
                properties.iter().for_each(|(key, value)| {
                    // should be in sync with mvt::tilevalue_from_json
                    let val_type = match value {
                        serde_json::Value::String(_) => "string",
                        serde_json::Value::Number(n) => {
                            if n.is_f64() {
                                "double"
                            } else if n.is_i64() {
                                "signed integer"
                            } else if n.is_u64() {
                                "unsigned integer"
                            } else {
                                // TODO: check
                                unreachable!()
                            }
                        }
                        serde_json::Value::Bool(_) => "boolean",
                        _ => "string",
                    };
                    fields.insert(key.to_string(), val_type.to_string());
                });
            }
            vec![tilejson::VectorLayer::new(layer_name.to_string(), fields)]
        }
        geojson::GeoJson::FeatureCollection(feature_collection) => {
            for feature in &feature_collection.features {
                if let Some(properties) = feature.properties.as_ref() {
                    properties.iter().for_each(|(key, value)| {
                        // should be in sync with mvt::tilevalue_from_json
                        let val_type = match value {
                            serde_json::Value::String(_) => "string",
                            serde_json::Value::Number(n) => {
                                if n.is_f64() {
                                    "double"
                                } else if n.is_i64() {
                                    "signed integer"
                                } else if n.is_u64() {
                                    "unsigned integer"
                                } else {
                                    // TODO: check
                                    unreachable!()
                                }
                            }
                            serde_json::Value::Bool(_) => "boolean",
                            _ => "string",
                        };
                        fields.insert(key.to_string(), val_type.to_string());
                    });
                }
            }
            vec![tilejson::VectorLayer::new(layer_name.to_string(), fields)]
        }
    }
}
