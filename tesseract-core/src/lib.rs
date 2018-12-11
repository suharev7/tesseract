mod backend;
mod dataframe;
pub mod format;
pub mod names;
mod schema;
mod schema_config;
mod sql;
mod query;

use failure::{Error, format_err};

pub use self::backend::Backend;
pub use self::dataframe::{DataFrame, Column, ColumnData};
use self::names::{
    Cut,
    Drilldown,
    Measure,
    Property,
};
pub use self::schema::{Schema, Cube};
use self::schema_config::SchemaConfig;
use self::sql::{
    CutSql,
    DrilldownSql,
    MeasureSql,
    MemberType,
    TableSql,
    LevelColumn,
};
pub use self::query::Query;


impl Schema {
    pub fn from_json(raw_schema: &str) -> Result<Self, Error> {
        let schema_config = serde_json::from_str::<SchemaConfig>(raw_schema)?;

        Ok(schema_config.into())
    }

    pub fn cube_metadata(&self, cube_name: &str) -> Option<Cube> {
        // Takes the first cube with the name.
        // TODO we still have to check that the cube names are distinct
        // before this.
        self.cubes.iter().find(|c| c.name == cube_name).cloned()
    }

    pub fn sql_query(
        &self,
        cube: &str,
        query: &Query,
        db: Database,
        ) -> Result<(String, Vec<String>), Error>
    {
        // First do checks, like making sure there's a measure, and that there's
        // either a cut or drilldown
        if query.measures.is_empty() {
            return Err(format_err!("No measure found; please specify at least one"));
        }
        if query.drilldowns.is_empty() && query.cuts.is_empty(){
            return Err(format_err!("Either a drilldown or cut is required"));
        }
        // also check that properties have a matching drilldown
        for property in &query.properties {
            let has_drill = query.drilldowns.iter()
                .any(|d| d.0 == property.level_name);

            if !has_drill {
                return Err(format_err!("Property {} has no matching drilldown", property));
            }
        }

        // now get the database metadata
        let table = self.cube_table(&cube)
            .ok_or(format_err!("No table found for cube {}", cube))?;

        let cut_cols = self.cube_cut_cols(&cube, &query.cuts)
            .map_err(|err| format_err!("Error getting cut cols: {}", err))?;

        let drill_cols = self.cube_drill_cols(&cube, &query.drilldowns, &query.properties, query.parents)
            .map_err(|err| format_err!("Error getting drill cols: {}", err))?;

        let mea_cols = self.cube_mea_cols(&cube, &query.measures)
            .map_err(|err| format_err!("Error getting mea cols: {}", err))?;

        // getting headers, not for sql but needed for formatting
        let drill_headers = self.cube_drill_headers(&cube, &query.drilldowns, &query.properties, query.parents)
            .map_err(|err| format_err!("Error getting drill heaers: {}", err))?;

        let mea_headers = self.cube_mea_headers(&cube, &query.measures)
            .map_err(|err| format_err!("Error getting mea cols: {}", err))?;

        let headers = [&drill_headers[..], &mea_headers[..]].concat();

        // now feed the database metadata into the sql generator
        match db {
            Database::Clickhouse => {
                Ok((
                    sql::clickhouse_sql(
                    table,
                    &cut_cols,
                    &drill_cols,
                    &mea_cols,
                    ),
                    headers,
                ))
            },
            Database::MySql => {
                Ok((
                    sql::clickhouse_sql(
                    table,
                    &cut_cols,
                    &drill_cols,
                    &mea_cols,
                    ),
                    headers,
                ))
            }
        }
    }

    //pub fn post_calculations(cal: &Calculations, df: DataFrame) -> DataFrame {
    //}
}

impl Schema {
    fn cube_table(&self, cube_name: &str) -> Option<TableSql> {
        self.cubes.iter()
            .find(|cube| &cube.name == &cube_name)
            .map(|cube| {
                TableSql {
                    name: cube.table.name.clone(),
                    primary_key: cube.table.primary_key.clone(),
                }
            })
    }

    fn cube_cut_cols(&self, cube_name: &str, cuts: &[Cut]) -> Result<Vec<CutSql>, Error> {
        let cube = self.cubes.iter()
            .find(|cube| &cube.name == &cube_name)
            .ok_or(format_err!("Could not find cube"))?;

        let mut res = vec![];

        for cut in cuts {
            let dim = cube.dimensions.iter()
                .find(|dim| dim.name == cut.level_name.dimension)
                .ok_or(format_err!("could not find dimension for cut"))?;
            let hier = dim.hierarchies.iter()
                .find(|hier| hier.name == cut.level_name.hierarchy)
                .ok_or(format_err!("could not find hierarchy for cut"))?;
            let level = hier.levels.iter()
                .find(|lvl| lvl.name == cut.level_name.level)
                .ok_or(format_err!("could not find level for cut"))?;

            // No table (means inline table) will replace with fact table
            let table = hier.table
                .clone()
                .unwrap_or(cube.table.clone());

            // primary key is currently required in hierarchy. because inline dim is not yet
            // allowed
            let primary_key = hier.primary_key.clone();

            let foreign_key = dim.foreign_key
                .clone()
                .ok_or(format_err!("No foreign key; it's required for now (until inline dim implemented)"))?;

            let column = level.key_column.clone();

            res.push(CutSql {
                table,
                primary_key,
                foreign_key,
                column,
                members: cut.members.clone(),
                member_type: dim.foreign_key_type.clone().unwrap_or(MemberType::NonText),
            });
        }

        Ok(res)
    }

    // TODO as currently written, properties that don't get picked up by a drilldown
    // will just silently fail.
    fn cube_drill_cols(
        &self,
        cube_name: &str,
        drills: &[Drilldown],
        properties: &[Property],
        parents: bool,
        ) -> Result<Vec<DrilldownSql>, Error>
    {
        let cube = self.cubes.iter()
            .find(|cube| &cube.name == &cube_name)
            .ok_or(format_err!("Could not find cube"))?;

        let mut res = vec![];

        // now iterate throw drill/property tuples
        for drill in drills {
            let dim = cube.dimensions.iter()
                .find(|dim| dim.name == drill.0.dimension)
                .ok_or(format_err!("could not find dimension for drill"))?;
            let hier = dim.hierarchies.iter()
                .find(|hier| hier.name == drill.0.hierarchy)
                .ok_or(format_err!("could not find hierarchy for drill"))?;
            let levels = &hier.levels;

            // for this drill, get related properties.
            // - filter by properties for this drilldown
            // - for each property, get the level
            let property_columns: Result<Vec<_>, _>= properties.iter()
                .filter(|p| p.level_name == drill.0)
                .map(|p| {
                    levels.iter()
                        .find(|lvl| lvl.name == p.level_name.level)
                        .and_then(|lvl| {
                            if let Some(ref properties) = lvl.properties {
                                properties.iter()
                                    .find(|schema_p| schema_p.name == p.property)

                            } else {
                                None
                            }
                        })
                        .map(|p| p.column.clone())
                        .ok_or(format_err!("cannot find property for {}", p))
                })
                .collect();
            let property_columns = property_columns?;

            // No table (means inline table) will replace with fact table
            let table = hier.table
                .clone()
                .unwrap_or(cube.table.clone());

            // primary key is currently required in hierarchy. because inline dim is not yet
            // allowed
            let primary_key = hier.primary_key.clone();

            let foreign_key = dim.foreign_key
                .clone()
                .ok_or(format_err!("No foreign key; it's required for now (until inline dim implemented)"))?;

            // logic for getting level columns.
            // if parents = true, then get all columns down to level
            // if not,then just level
            let level_idx = levels.iter()
                .position(|lvl| lvl.name == drill.0.level)
                .ok_or(format_err!("could not find hierarchy for drill"))?;

            let mut level_columns = vec![];

            if parents {
                for i in 0..=level_idx {
                    level_columns.push(LevelColumn {
                        key_column: levels[i].key_column.clone(),
                        name_column: levels[i].name_column.clone(),
                    });
                }
            } else {
                level_columns.push(LevelColumn {
                    key_column: levels[level_idx].key_column.clone(),
                    name_column: levels[level_idx].name_column.clone(),
                });
            }

            res.push(DrilldownSql {
                table,
                primary_key,
                foreign_key,
                level_columns,
                property_columns,
            });
        }

        Ok(res)
    }

    fn cube_mea_cols(&self, cube_name: &str, meas: &[Measure]) -> Result<Vec<MeasureSql>, Error> {
        let cube = self.cubes.iter()
            .find(|cube| &cube.name == &cube_name)
            .ok_or(format_err!("Could not find cube"))?;

        let mut res = vec![];

        for measure in meas {
            let mea = cube.measures.iter()
                .find(|m| m.name == measure.0)
                .ok_or(format_err!("could not find dimension for drill"))?;

            res.push(MeasureSql {
                column: mea.column.clone(),
                aggregator: mea.aggregator.clone(),
            });
        }

        Ok(res)
    }

    /// order should mirror DrillSql col_string,
    /// which should be levels first and then properties after
    /// (for each drilldown)
    fn cube_drill_headers(
        &self,
        cube_name: &str,
        drills: &[Drilldown],
        properties: &[Property],
        parents: bool,
        ) -> Result<Vec<String>, Error>
    {
        let cube = self.cubes.iter()
            .find(|cube| &cube.name == &cube_name)
            .ok_or(format_err!("Could not find cube"))?;

        let mut level_headers = vec![];

        for drill in drills {
            let dim = cube.dimensions.iter()
                .find(|dim| dim.name == drill.0.dimension)
                .ok_or(format_err!("could not find dimension for drill"))?;
            let hier = dim.hierarchies.iter()
                .find(|hier| hier.name == drill.0.hierarchy)
                .ok_or(format_err!("could not find hierarchy for drill"))?;
            let levels = &hier.levels;

            // logic for getting level names.
            // if parents = true, then get all columns down to level
            // if not,then just level name
            let level_idx = hier.levels.iter()
                .position(|lvl| lvl.name == drill.0.level)
                .ok_or(format_err!("could not find hierarchy for drill"))?;


            // In this section, need to watch out for whether there's both a
            // key column and a name column and add ID to the first if necessary
            if parents {
                for i in 0..=level_idx {
                    if levels[i].name_column.is_some() {
                        level_headers.push(levels[i].name.clone() + " ID");
                    }
                    level_headers.push(levels[i].name.clone());
                }
            } else {
                if levels[level_idx].name_column.is_some() {
                    level_headers.push(levels[level_idx].name.clone() + " ID");
                }
                level_headers.push(levels[level_idx].name.clone());
            }

            // for this drill, get related properties.
            // - filter by properties for this drilldown
            // - for each property, get the level
            let property_columns: Result<Vec<_>, _>= properties.iter()
                .filter(|p| p.level_name == drill.0)
                .map(|p| {
                    levels.iter()
                        .find(|lvl| lvl.name == p.level_name.level)
                        .and_then(|lvl| {
                            if let Some(ref properties) = lvl.properties {
                                properties.iter()
                                    .find(|schema_p| schema_p.name == p.property)

                            } else {
                                None
                            }
                        })
                        .map(|p| p.name.clone())
                        .ok_or(format_err!("cannot find property for {}", p))
                })
                .collect();
            let property_columns = property_columns?;

            level_headers.extend(property_columns);
        }

        Ok(level_headers)
    }

    fn cube_mea_headers(&self, cube_name: &str, meas: &[Measure]) -> Result<Vec<String>, Error> {
        let cube = self.cubes.iter()
            .find(|cube| &cube.name == &cube_name)
            .ok_or(format_err!("Could not find cube"))?;

        let mut res = vec![];

        for measure in meas {
            let mea = cube.measures.iter()
                .find(|m| m.name == measure.0)
                .ok_or(format_err!("could not find measure in cube"))?;

            res.push(mea.name.clone());
        }

        Ok(res)
    }
}

pub enum Database {
    Clickhouse,
    MySql
}
