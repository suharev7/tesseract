use std::collections::HashMap;
use std::str;

use actix_web::{
    AsyncResponder,
    FutureResponse,
    HttpRequest,
    HttpResponse,
    Path,
};
use failure::{Error, format_err, bail};
use futures::future::*;
use lazy_static::lazy_static;
use log::*;
use serde_qs as qs;
use serde_derive::Deserialize;
use url::Url;

use tesseract_core::names::{Cut, Drilldown, Property, Measure, LevelName, Mask};
use tesseract_core::format::{format_records, FormatType};
use tesseract_core::query::{FilterQuery, GrowthQuery, RcaQuery, TopQuery, RateQuery};
use tesseract_core::{Query as TsQuery, MeaOrCalc, DataFrame, Column, ColumnData, is_same_columndata_type};
use tesseract_core::schema::{Cube, DimensionType};

use crate::app::AppState;
use crate::errors::ServerError;
use crate::logic_layer::{LogicLayerConfig, CubeCache, Time};
use crate::util::boxed_error;
use super::super::util;
use crate::handlers::logic_layer::{query_geoservice, GeoserviceQuery};


/// Handles default aggregation when a format is not specified.
/// Default format is CSV.
pub fn logic_layer_default_handler(
    (req, _cube): (HttpRequest<AppState>, Path<()>)
) -> FutureResponse<HttpResponse>
{
    logic_layer_aggregation(req, "jsonrecords".to_owned())
}


/// Handles aggregation when a format is specified.
pub fn logic_layer_handler(
    (req, cube_format): (HttpRequest<AppState>, Path<(String)>)
) -> FutureResponse<HttpResponse>
{
    logic_layer_aggregation(req, cube_format.to_owned())
}


#[derive(Debug, Clone, Deserialize)]
pub struct LogicLayerQueryOpt {
    pub cube: String,
    pub drilldowns: Option<String>,
    #[serde(flatten)]
    pub cuts: Option<HashMap<String, String>>,
    pub time: Option<String>,
    measures: Option<String>,
    properties: Option<String>,
    filters: Option<String>,
    parents: Option<bool>,
    top: Option<String>,
    top_where: Option<String>,
    sort: Option<String>,
    limit: Option<String>,
    growth: Option<String>,
    rca: Option<String>,
    debug: Option<bool>,
    exclude_default_members: Option<bool>,
    locale: Option<String>,
    //    distinct: Option<bool>,
    //    nonempty: Option<bool>,
    sparse: Option<bool>,
    rate: Option<String>,
}


impl LogicLayerQueryOpt {
    pub fn deserialize_args(arg: String) -> Vec<String> {
        let mut open = false;
        let mut curr_str = "".to_string();
        let mut arg_vec: Vec<String> = vec![];

        for c in arg.chars() {
            let c_str = c.to_string();

            if c_str == "[" {
                open = true;
            } else if c_str == "]" {
                open = false;
            } else if c_str == "," {
                if open {
                    curr_str += &c_str;
                } else {
                    arg_vec.push(curr_str.clone());
                    curr_str = "".to_string();
                }
            } else {
                curr_str += &c_str;
            }
        }

        if curr_str.len() >= 1 {
            arg_vec.push(curr_str.clone());
        }

        arg_vec
    }
}


/// Performs data aggregation.
pub fn logic_layer_aggregation(
    req: HttpRequest<AppState>,
    format: String,
) -> FutureResponse<HttpResponse>
{
    let format = format.parse::<FormatType>();
    let format = match format {
        Ok(f) => f,
        Err(err) => return boxed_error(err.to_string()),
    };

    info!("Format: {:?}", format);

    let query = req.query_string();
    let schema = req.state().schema.read().unwrap();
    let debug = req.state().debug;

    let logic_layer_config: Option<LogicLayerConfig> = match &req.state().logic_layer_config {
        Some(llc) => Some(llc.read().unwrap().clone()),
        None => None
    };

    lazy_static!{
        static ref QS_NON_STRICT: qs::Config = qs::Config::new(5, false);
    }

    let agg_query = match QS_NON_STRICT.deserialize_str::<LogicLayerQueryOpt>(query) {
        Ok(q) => q,
        Err(err) => return boxed_error(err.to_string())
    };

    // Check to see if the logic layer config has a alias with the
    // provided cube name
    let cube_name = match logic_layer_config.clone() {
        Some(llc) => {
            match llc.substitute_cube_name(agg_query.cube.clone()) {
                Ok(cn) => cn,
                Err(_) => agg_query.cube.clone()
            }
        },
        None => agg_query.cube.clone()
    };

    let cube = match schema.get_cube_by_name(&cube_name) {
        Ok(c) => c,
        Err(err) => return boxed_error(err.to_string())
    };

    let cube_cache = match req.state().cache.read().unwrap().find_cube_info(&cube_name) {
        Some(cube_cache) => cube_cache,
        None => return boxed_error("Unable to access cube cache".to_string())
    };

    info!("Aggregate query: {:?}", agg_query);

    // Turn AggregateQueryOpt into TsQuery
    let ts_queries = generate_ts_queries(
        agg_query.clone(), &cube, &cube_cache,
        &logic_layer_config, &req.state().env_vars.geoservice_url
    );
    let (ts_queries, header_map) = match ts_queries {
        Ok((ts_queries, header_map)) => (ts_queries, header_map),
        Err(err) => return boxed_error(err.to_string())
    };

    if ts_queries.len() == 0 {
        return boxed_error("Unable to generate queries".to_string())
    }

    let mut sql_strings: Vec<String> = vec![];
    let mut final_headers: Vec<String> = vec![];

    for ts_query in &ts_queries {
        debug!("Tesseract query: {:?}", ts_query);

        let query_ir_headers = req
            .state()
            .schema.read().unwrap()
            .sql_query(&cube_name, &ts_query);

        let (query_ir, headers) = match query_ir_headers {
            Ok(x) => x,
            Err(err) => return boxed_error(err.to_string())
        };

        debug!("Query IR: {:?}", query_ir);

        let sql = req.state()
            .backend
            .generate_sql(query_ir);

        debug!("SQL query: {}", sql);

        // Substitute header names (only need to do this once)
        if final_headers.len() == 0 {
            for header in &headers {
                let mut new_header = header.clone();

                for (k, v) in header_map.iter() {
                    if header.contains(k) {
                        new_header = new_header.replace(k, v);
                    }
                }

                final_headers.push(new_header);
            }
        }

        sql_strings.push(sql);
    }

    debug!("Headers: {:?}", final_headers);

    // Joins all the futures for each TsQuery
    let futs: JoinAll<Vec<Box<Future<Item=DataFrame, Error=Error>>>> = join_all(sql_strings
            .iter()
            .map(|sql| {
                req.state()
                    .backend
                    .exec_sql(sql.clone())
            })
            .collect()
        );

    // Process data received once all futures are resolved and return response
    futs
        .and_then(move |dfs| {
            let mut final_columns: Vec<Column> = vec![];

            let num_cols = match dfs.get(0) {
                Some(df) => df.columns.len(),
                None => return Err(format_err!("No dataframes were returned."))
            };

            for col_i in 0..num_cols {
                let mut same_type = true;

                let first_col: &Column = match &dfs[0].columns.get(col_i) {
                    Some(col) => col,
                    None => return Err(format_err!("Unable to index column."))
                };

                for df in &dfs {
                    if !is_same_columndata_type(&first_col.column_data, &df.columns[col_i].column_data) {
                        same_type = false;
                        break;
                    }
                }

                let mut col_data: Vec<String> = vec![];

                for df in &dfs {
                    let c: &Column = &df.columns[col_i];
                    let rows = c.stringify_column_data();
                    col_data = [&col_data[..], &rows[..]].concat()
                }

                if same_type {
                    let mut column_data: ColumnData = ColumnData::Text(col_data.clone());

                    // TODO: Process nullable columns
                    match first_col.column_data {
                        ColumnData::Int8(_) => {
                            column_data = ColumnData::Int8(col_data.iter().map(|x| x.parse::<i8>().unwrap()).collect());
                        },
                        ColumnData::Int16(_) => {
                            column_data = ColumnData::Int16(col_data.iter().map(|x| x.parse::<i16>().unwrap()).collect());
                        },
                        ColumnData::Int32(_) => {
                            column_data = ColumnData::Int32(col_data.iter().map(|x| x.parse::<i32>().unwrap()).collect());
                        },
                        ColumnData::Int64(_) => {
                            column_data = ColumnData::Int64(col_data.iter().map(|x| x.parse::<i64>().unwrap()).collect());
                        },
                        ColumnData::UInt8(_) => {
                            column_data = ColumnData::UInt8(col_data.iter().map(|x| x.parse::<u8>().unwrap()).collect());
                        },
                        ColumnData::UInt16(_) => {
                            column_data = ColumnData::UInt16(col_data.iter().map(|x| x.parse::<u16>().unwrap()).collect());
                        },
                        ColumnData::UInt32(_) => {
                            column_data = ColumnData::UInt32(col_data.iter().map(|x| x.parse::<u32>().unwrap()).collect());
                        },
                        ColumnData::UInt64(_) => {
                            column_data = ColumnData::UInt64(col_data.iter().map(|x| x.parse::<u64>().unwrap()).collect());
                        },
                        ColumnData::Float32(_) => {
                            column_data = ColumnData::Float32(col_data.iter().map(|x| x.parse::<f32>().unwrap()).collect());
                        },
                        ColumnData::Float64(_) => {
                            column_data = ColumnData::Float64(col_data.iter().map(|x| x.parse::<f64>().unwrap()).collect());
                        },
                        _ => ()
                    }

                    final_columns.push(Column {
                        name: "placeholder".to_string(),
                        column_data
                    });
                } else {
                    final_columns.push(Column {
                        name: "placeholder".to_string(),
                        column_data: ColumnData::Text(col_data)
                    });
                }
            }

            let final_df = DataFrame { columns: final_columns };

            let content_type = util::format_to_content_type(&format);

            match format_records(&final_headers, final_df, format) {
                Ok(res) => {
                    Ok(HttpResponse::Ok()
                        .set(content_type)
                        .body(res))
                },
                Err(err) => Ok(HttpResponse::NotFound().json(err.to_string())),
            }
        })
        .map_err(move |e| {
            if debug {
                ServerError::Db { cause: e.to_string() }.into()
            } else {
                ServerError::Db { cause: "Internal Server Error 1010".to_owned() }.into()
            }
        })
        .responder()
}


/// Generates a series of Tesseract queries from a single LogicLayerQueryOpt.
/// This function contains the bulk of the logic layer logic.
pub fn generate_ts_queries(
        agg_query_opt: LogicLayerQueryOpt,
        cube: &Cube,
        cube_cache: &CubeCache,
        ll_config: &Option<LogicLayerConfig>,
        geoservice_url: &Option<Url>
) -> Result<(Vec<TsQuery>, HashMap<String, String>), Error> {

    let level_map = &cube_cache.level_map;
    let property_map = &cube_cache.property_map;

    let mut captions: Vec<Property> = vec![];
    let locales: Vec<String> = match &agg_query_opt.locale {
        Some(locale) => locale.split(",").map(|s| s.to_string()).collect(),
        None => vec![]
    };

    let mut cuts_map = clean_cuts_map(&agg_query_opt, &cube_cache, &ll_config)?;

    let parents = agg_query_opt.parents.unwrap_or(false);

    let drilldowns: Vec<_> = agg_query_opt.drilldowns
        .map(|ds| {
            let mut drilldowns: Vec<Drilldown> = vec![];

            for level_value in LogicLayerQueryOpt::deserialize_args(ds) {
                // Check logic layer config for any named set substitutions
                let level_key = match ll_config.clone() {
                    Some(ll_conf) => {
                        match ll_conf.substitute_drill_value(level_value.clone()) {
                            Some(ln) => {
                                cuts_map
                                    .entry(ln.clone())
                                    .or_insert(level_value.clone());
                                ln
                            },
                            None => level_value.clone()
                        }
                    },
                    None => level_value.clone()
                };

                let level_name = match level_map.get(&level_key) {
                    Some(level_name) => level_name,
                    None => break
                };

                let level = match cube.get_level(level_name) {
                    Some(level) => level,
                    None => break
                };

                drilldowns.push(Drilldown(level_name.clone()));

                // Check for captions for this level
                let new_captions = level.get_captions(&level_name, &locales);
                captions.extend_from_slice(&new_captions);

                // If `parents`, check captions for parent levels
                if parents {
                     let new_captions = get_parent_captions(&cube, &level_name, &locales);
                     captions = [&captions[..], &new_captions[..]].concat();
                }
            }

            drilldowns
        })
        .unwrap_or(vec![]);

    let measures: Vec<_> = agg_query_opt.measures
        .map(|ms| {
            let mut measures: Vec<Measure> = vec![];

            for measure in LogicLayerQueryOpt::deserialize_args(ms) {
                let m = match measure.parse() {
                    Ok(m) => m,
                    Err(_) => break
                };
                measures.push(m);
            }

            measures
        })
        .unwrap_or(vec![]);

    let properties: Vec<_> = agg_query_opt.properties
        .map(|ps| {
            let mut properties: Vec<Property> = vec![];

            for property_value in LogicLayerQueryOpt::deserialize_args(ps) {
                // TODO: Break or bail?
                let property = match property_map.get(&property_value) {
                    Some(p) => p,
                    None => break
                };

                properties.push(property.clone());
            }

            properties
        })
        .unwrap_or(vec![]);

    // TODO: Implement
    let filters: Vec<FilterQuery>= vec![];

    let top: Option<TopQuery> = agg_query_opt.top.clone()
        .map(|t| {
            let top_split: Vec<String> = t.split(',').map(|s| s.to_string()).collect();

            if top_split.len() != 4 {
                return Err(format_err!("Bad formatting for top param."));
            }

            let level_name = match level_map.get(&top_split[1]) {
                Some(l) => l,
                None => bail!("Unable to find top level")
            };

            let mea_or_calc: MeaOrCalc = top_split[2].parse()?;

            Ok(TopQuery::new(
                top_split[0].parse()?,
                level_name.clone(),
                vec![mea_or_calc],
                top_split[3].parse()?
            ))
        })
        .transpose()?;
    let top_where = agg_query_opt.top_where
        .map(|t| t.parse())
        .transpose()?;
    let sort = agg_query_opt.sort
        .map(|s| s.parse())
        .transpose()?;
    let limit = agg_query_opt.limit
        .map(|l| l.parse())
        .transpose()?;

    let growth = match agg_query_opt.growth {
        Some(g) => {
            let gro_split: Vec<String> = g.split(',').map(|s| s.to_string()).collect();

            if gro_split.len() == 1 {
                return Err(format_err!("Please provide a growth measure name."));
            } else if gro_split.len() != 2 {
                return Err(format_err!("Bad formatting for growth param."));
            }

            let level_key = gro_split[0].clone();
            let measure = gro_split[1].clone();

            let level_name = match level_map.get(&level_key) {
                Some(l) => l,
                None => bail!("Unable to find growth level")
            };

            let growth = GrowthQuery::new(
                level_name.dimension.clone(),
                level_name.hierarchy.clone(),
                level_name.level.clone(),
                measure
            );

            Some(growth)
        },
        None => None
    };

    let rca = match agg_query_opt.rca {
        Some(r) => {
            let rca_split: Vec<String> = r.split(",").map(|s| s.to_string()).collect();

            if rca_split.len() != 3 {
                return Err(format_err!("Bad formatting for RCA param."));
            }

            let drill1_level_key = rca_split[0].clone();
            let drill2_level_key = rca_split[1].clone();
            let measure = rca_split[2].clone();

            let level_name_1 = match level_map.get(&drill1_level_key) {
                Some(l) => l,
                None => bail!("Unable to find drill 1 level")
            };

            let level_name_2 = match level_map.get(&drill2_level_key) {
                Some(l) => l,
                None => bail!("Unable to find drill 2 level")
            };

            let rca = RcaQuery::new(
                level_name_1.dimension.clone(),
                level_name_1.hierarchy.clone(),
                level_name_1.level.clone(),
                level_name_2.dimension.clone(),
                level_name_2.hierarchy.clone(),
                level_name_2.level.clone(),
                measure
            );

            Some(rca)
        },
        None => None
    };

    // TODO: Resolve named sets
    let rate = match agg_query_opt.rate {
        Some(rate) => {
            let level_value_split: Vec<String> = rate.split('.').map(|s| s.to_string()).collect();

            if level_value_split.len() != 2 {
                bail!("Bad formatting for rate calculation.");
            }

            let level_name = match level_map.get(&level_value_split[0]) {
                Some(level_name) => level_name.clone(),
                None => bail!("Unrecognized level in rate calculation.")
            };
            let value = level_value_split[1].clone();

            let values: Vec<String> = value.split(",").map(|s| s.to_string()).collect();

            Some(RateQuery::new(level_name, values))
        },
        None => None
    };

    let debug = agg_query_opt.debug.unwrap_or(false);
    let sparse = agg_query_opt.sparse.unwrap_or(false);
    let exclude_default_members = agg_query_opt.exclude_default_members.unwrap_or(false);

    // This is where all the different queries are ACTUALLY generated.
    // Everything before this is common to all queries being generated.

    let (dimension_cuts_map, header_map) = resolve_cuts(
        &cuts_map, &cube, &cube_cache, &level_map, &property_map, &geoservice_url
    )?;

    // Groups together cuts for the same dimension
    // This is needed so we can generate all the possible cut combinations in the next step
    let mut dimension_cuts: Vec<Vec<Cut>> = vec![];

    // Need to add drilldowns for cuts on these levels
    // This will be done in the next step
    let mut added_drilldowns: Vec<LevelName> = vec![];

    // Populate the vectors above
    for (_dimension_name, level_cuts_map) in dimension_cuts_map.iter() {
        let mut inner_cuts: Vec<Cut> = vec![];

        let num_level_cuts = level_cuts_map.len();

        for (level_name, level_cuts) in level_cuts_map.iter() {
            let cut = Cut {
                level_name: level_name.clone(),
                members: level_cuts.clone(),
                mask: Mask::Include,
                for_match: false
            };

            inner_cuts.push(cut.clone());

            if num_level_cuts > 1 {
                // We're doing multiple cuts on this dimension
                added_drilldowns.push(cut.level_name.clone());
            }
        }

        dimension_cuts.push(inner_cuts);
    }

    // All the different TsQuery's that need to be performed
    let mut queries: Vec<TsQuery> = vec![];

    // Get all possible combinations of cuts across dimensions
    let cut_combinations: Vec<Vec<Cut>> = cartesian_product(dimension_cuts);

    if cut_combinations.len() == 0 {
        queries.push(TsQuery {
            drilldowns: drilldowns.clone(),
            cuts: vec![],
            measures: measures.clone(),
            parents: parents.clone(),
            properties: properties.clone(),
            captions: captions.clone(),
            top: top.clone(),
            top_where: top_where.clone(),
            sort: sort.clone(),
            limit: limit.clone(),
            rca: rca.clone(),
            growth: growth.clone(),
            debug: debug.clone(),
            exclude_default_members: exclude_default_members.clone(),
            filters: filters.clone(),
            rate: rate.clone(),
            sparse: sparse.clone(),
        });
    } else {
        // Create a TsQuery for each cut combination
        for cut_combination in &cut_combinations {
            let mut drills = drilldowns.clone();
            let mut caps = captions.clone();

            for cut in cut_combination.clone() {
                // Look for drilldowns that might need to be added
                if added_drilldowns.contains(&cut.level_name) {
                    drills.push(Drilldown(cut.level_name.clone()));

                    let level = match cube.get_level(&cut.level_name) {
                        Some(level) => level,
                        None => break
                    };

                    // Add captions for this level
                    let new_captions = level.get_captions(&cut.level_name, &locales);
                    caps.extend_from_slice(&new_captions);
                }
            }

            // Populate queries vector
            queries.push(TsQuery {
                drilldowns: drills,
                cuts: cut_combination.clone(),
                measures: measures.clone(),
                parents: parents.clone(),
                properties: properties.clone(),
                captions: caps,
                top: top.clone(),
                top_where: top_where.clone(),
                sort: sort.clone(),
                limit: limit.clone(),
                rca: rca.clone(),
                growth: growth.clone(),
                debug: debug.clone(),
                exclude_default_members: exclude_default_members.clone(),
                filters: filters.clone(),
                rate: rate.clone(),
                sparse: sparse.clone(),
            });
        }
    }

    Ok((queries, header_map))

}


/// Given a vector containing a partial Cartesian product, and a list of items,
/// return a vector adding the list of items to the partial Cartesian product.
/// From: https://gist.github.com/kylewlacy/115965b40e02a3325558
pub fn partial_cartesian<T: Clone>(a: Vec<Vec<T>>, b: Vec<T>) -> Vec<Vec<T>> {
    a.into_iter().flat_map(|xs| {
        b.iter().cloned().map(|y| {
            let mut vec = xs.clone();
            vec.push(y);
            vec
        }).collect::<Vec<_>>()
    }).collect()
}


/// Computes the Cartesian product of lists[0] * lists[1] * ... * lists[n].
/// From: https://gist.github.com/kylewlacy/115965b40e02a3325558
pub fn cartesian_product<T: Clone>(lists: Vec<Vec<T>>) -> Vec<Vec<T>> {
    match lists.split_first() {
        Some((first, rest)) => {
            let init: Vec<Vec<T>> = first.iter().cloned().map(|n| vec![n]).collect();

            rest.iter().cloned().fold(init, |vec, list| {
                partial_cartesian(vec, list)
            })
        },
        None => {
            vec![]
        }
    }
}


/// Performs named set and time substitutions in the original cuts HashMap
/// deserialized from the query.
pub fn clean_cuts_map(
        agg_query_opt: &LogicLayerQueryOpt,
        cube_cache: &CubeCache,
        ll_config: &Option<LogicLayerConfig>
) -> Result<HashMap<String, String>, Error> {
    // Holds a mapping from cut keys (dimensions or levels) to values
    let mut agg_query_opt_cuts = match &agg_query_opt.cuts {
        Some(c) => c.clone(),
        None => HashMap::new()
    };

    // Process `time` param (latest/oldest)
    match &agg_query_opt.time {
        Some(time_param) => {
            let time_cuts: Vec<String> = time_param.split(",").map(|s| s.to_string()).collect();

            for time_cut in time_cuts {
                let tc: Vec<String> = time_cut.split(".").map(|s| s.to_string()).collect();

                if tc.len() != 2 {
                    return Err(format_err!("Malformatted time cut"));
                }

                let time = match Time::from_key_value(tc[0].clone(), tc[1].clone()) {
                    Ok(time) => time,
                    Err(err) => return Err(format_err!("{}", err.to_string()))
                };

                let (cut, cut_value) = match cube_cache.get_time_cut(time) {
                    Ok(cut) => cut,
                    Err(err) => return Err(format_err!("{}", err.to_string()))
                };

                agg_query_opt_cuts.insert(cut, cut_value);
            }
        },
        None => ()
    };

    // Find and perform any named set substitutions
    for (cut_key, cut_values) in agg_query_opt_cuts.clone().iter() {
        if cut_values.is_empty() {
            continue;
        }

        let mut final_cuts: Vec<String> = vec![];

        let cut_values_split: Vec<String> = cut_values.split(",").map(|s| s.to_string()).collect();

        for cut_value in &cut_values_split {
            match ll_config.clone() {
                Some(ll_conf) => {
                    let new_cut_values = ll_conf.substitute_cut(cut_key.clone(), cut_value.clone());

                    if &new_cut_values != cut_value {
                        let new_cut_values_split: Vec<String> = new_cut_values.split(",").map(|s| s.to_string()).collect();

                        final_cuts = [&final_cuts[..], &new_cut_values_split[..]].concat();
                    } else {
                        final_cuts.push(new_cut_values.clone());
                    }
                },
                None => {
                    final_cuts.push(cut_value.clone());
                }
            };
        }

        *agg_query_opt_cuts.get_mut(cut_key).unwrap() = final_cuts.join(",");
    }

    Ok(agg_query_opt_cuts)
}


/// Implements logic to resolve logic layer cuts (including those with operations)
/// into a HashMap separating cuts for each dimension. Doing so helps generate all
/// the possible cut combinations in the next step.
/// This method also returns a HashMap of header name substitutions that will help
/// with the naming of the final column names in the response.
pub fn resolve_cuts(
        cuts_map: &HashMap<String, String>,
        cube: &Cube,
        cube_cache: &CubeCache,
        level_map: &HashMap<String, LevelName>,
        property_map: &HashMap<String, Property>,
        geoservice_url: &Option<Url>
) -> Result<(HashMap<String, HashMap<LevelName, Vec<String>>>, HashMap<String, String>), Error> {
    // HashMap of cuts for each dimension.
    // In the outer HashMap, the keys are dimension names as string and the
    // values are the inner hashmap. The inner HashMap's keys are level names
    // and the values are cut values for a given level.
    let mut dimension_cuts_map: HashMap<String, HashMap<LevelName, Vec<String>>> = HashMap::new();

    // Helps convert dataframe column names to their equivalent dimension names.
    // The only exception to this logic is when there is a single cut for a
    // given dimension. In that case, we want to preserve the level name as the
    // final column name.
    let mut header_map: HashMap<String, String> = HashMap::new();

    // Keep track of which level names were matched to a level as opposed to a
    // dimension.
    let mut level_matches: Vec<LevelName> = vec![];

    for (cut_key, cut_values) in cuts_map.iter() {
        if cut_values.is_empty() {
            continue;
        }

        // Each of these cut_values needs to be matched to a `LevelName` object
        let cut_values: Vec<String> = cut_values.split(",").map(|s| s.to_string()).collect();

        for cut_value in &cut_values {
            let elements: Vec<String> = cut_value.clone().split(":").map(|s| s.to_string()).collect();

            let cut = match elements.get(0) {
                Some(cut) => cut,
                None => return Err(format_err!("Malformatted cut."))
            };

            // Check to see if this matches any dimension names
            // Get LevelName based on cut_key and element
            let mut level_name = match cube_cache.dimension_caches.get(cut_key) {
                Some(dimension_cache) => {
                    match dimension_cache.id_map.get(cut) {
                        Some(level_names) => {
                            if level_names.len() > 1 {
                                return Err(format_err!("{} matches multiple levels in this dimension.", cut))
                            }

                            match level_names.get(0) {
                                Some(ln) => ln.clone(),
                                None => return Err(format_err!("{} matches no levels in this dimension.", cut))
                            }
                        },
                        None => continue
                    }
                },
                None => {
                    match level_map.get(cut_key) {
                        Some(level_name) => {
                            level_matches.push(level_name.clone());
                            level_name.clone()
                        },
                        None => continue
                    }
                }
            };

            header_map.entry(level_name.level.clone()).or_insert(level_name.dimension.clone());

            if elements.len() == 1 {
                // Simply add this cut to the map
                dimension_cuts_map = add_cut_entries(dimension_cuts_map, &level_name, vec![cut.clone()]);
            } else if elements.len() == 2 {
                let operation = match elements.get(1) {
                    Some(operation) => operation.clone(),
                    None => return Err(format_err!("Unable to extract cut operation."))
                };

                if operation == "children".to_string() {

                    let child_level = match cube.get_child_level(&level_name)? {
                        Some(child_level) => child_level,
                        None => continue  // This level has no child
                    };

                    let child_level_name = LevelName {
                        dimension: level_name.dimension.clone(),
                        hierarchy: level_name.hierarchy.clone(),
                        level: child_level.name.clone()
                    };

                    // Will help convert the column name for this level to its dimension name
                    header_map.entry(child_level_name.level.clone()).or_insert(child_level_name.dimension.clone());

                    // Get children IDs from the cache
                    let level_cache = match cube_cache.level_caches.get(&level_name.level) {
                        Some(level_cache) => level_cache,
                        None => return Err(format_err!("Could not find cached entries for {}.", level_name.level))
                    };

                    let children_ids = match &level_cache.children_map {
                        Some(children_map) => {
                            match children_map.get(cut) {
                                Some(children_ids) => children_ids.clone(),
                                None => continue
                            }
                        },
                        None => continue
                    };

                    // Add children IDs to the `dimension_cuts_map`
                    dimension_cuts_map = add_cut_entries(dimension_cuts_map, &child_level_name, children_ids);

                } else if operation == "parents".to_string() {

                    let parent_levels = cube.get_level_parents(&level_name)?;

                    if parent_levels.is_empty() {
                        // This level has no parents
                        continue;
                    }

                    for parent_level in (parent_levels.iter()).rev() {
                        let parent_level_name = LevelName {
                            dimension: level_name.dimension.clone(),
                            hierarchy: level_name.hierarchy.clone(),
                            level: parent_level.name.clone()
                        };

                        header_map.entry(parent_level_name.level.clone()).or_insert(parent_level_name.dimension.clone());

                        // Get parent IDs from the cache
                        let level_cache = match cube_cache.level_caches.get(&level_name.level) {
                            Some(level_cache) => level_cache,
                            None => return Err(format_err!("Could not find cached entries for {}.", level_name.level))
                        };

                        let parent_id = match &level_cache.parent_map {
                            Some(parent_map) => {
                                match parent_map.get(cut) {
                                    Some(parent_id) => parent_id.clone(),
                                    None => continue
                                }
                            },
                            None => continue
                        };

                        // Add parent ID to the `dimension_cuts_map`
                        dimension_cuts_map = add_cut_entries(dimension_cuts_map, &parent_level_name, vec![parent_id]);

                        // Update current level_name for the next iteration
                        level_name = parent_level_name.clone();
                    }

                } else if operation == "neighbors".to_string() {

                    // Find dimension for the level name
                    let dimension = cube.get_dimension(&level_name)
                        .ok_or_else(|| format_err!("Could not find dimension for {}.", level_name.level))?;

                    match dimension.dim_type {
                        DimensionType::Geo => {
                            match geoservice_url {
                                Some(geoservice_url) => {
                                    let mut neighbors_ids: Vec<String> = vec![];

                                    let geoservice_response = query_geoservice(
                                        geoservice_url, &GeoserviceQuery::Neighbors, &cut
                                    )?;

                                    for res in &geoservice_response {
                                        neighbors_ids.push(res.geoid.clone());
                                    }

                                    // Add neighbors IDs to the `dimension_cuts_map`
                                    dimension_cuts_map = add_cut_entries(dimension_cuts_map, &level_name, neighbors_ids);
                                },
                                None => return Err(format_err!("Unable to perform geoservice request: A Geoservice URL has not been provided."))
                            };
                        },
                        _ => {
                            let level_cache = match cube_cache.level_caches.get(&level_name.level) {
                                Some(level_cache) => level_cache,
                                None => return Err(format_err!("Could not find cached entries for {}.", level_name.level))
                            };

                            let neighbors_ids = match level_cache.neighbors_map.get(cut) {
                                Some(neighbors_ids) => neighbors_ids.clone(),
                                None => continue
                            };

                            // Add neighbors IDs to the `dimension_cuts_map`
                            dimension_cuts_map = add_cut_entries(dimension_cuts_map, &level_name, neighbors_ids);
                        }
                    }

                } else {
                    return Err(format_err!("Unrecognized operation: `{}`.", operation));
                }
            } else {
                return Err(format_err!("Multiple cut operations are not supported on the same element."));
            }
        }
    }

    // Check if anything needs to be removed from the header_map
    for (_k1, level_name_map) in dimension_cuts_map.iter() {
        if level_name_map.len() == 1 {
            for (level_name, _v2) in level_name_map.iter() {
                if level_matches.contains(&level_name) {
                    header_map.remove_entry(&level_name.level);
                }
            }
        }
    }

    Ok((dimension_cuts_map, header_map))
}


/// Adds cut entries to the dimension_cuts_map HashMap.
pub fn add_cut_entries(
    mut dimension_cuts_map: HashMap<String, HashMap<LevelName, Vec<String>>>,
    level_name: &LevelName,
    elements: Vec<String>
) -> HashMap<String, HashMap<LevelName, Vec<String>>> {

    dimension_cuts_map.entry(level_name.dimension.clone()).or_insert(HashMap::new());
    let map_entry = dimension_cuts_map.get_mut(&level_name.dimension).unwrap();
    map_entry.entry(level_name.clone()).or_insert(vec![]);
    let level_cuts = map_entry.get_mut(&level_name).unwrap();

    // Add each element to the map
    for element in &elements {
        level_cuts.push(element.clone());
    }

    dimension_cuts_map

}


/// Helper to get all the relevant parent captions given a locales list.
pub fn get_parent_captions(cube: &Cube, level_name: &LevelName, locales: &Vec<String>) -> Vec<Property> {
    let mut captions: Vec<Property> = vec![];

    let level_parents = cube.get_level_parents(level_name).unwrap_or(vec![]);
    for parent_level in level_parents {
        if let Some(ref props) = parent_level.properties {
            for prop in props {
                if let Some(ref cap) = prop.caption_set {
                    for locale in locales {
                        if locale == cap {
                            captions.push(
                                Property::new(
                                    level_name.dimension.clone(),
                                    level_name.hierarchy.clone(),
                                    parent_level.name.clone(),
                                    prop.name.clone(),
                                )
                            )
                        }
                    }
                } else {
                    continue
                }
            }
        } else {
            continue
        }
    }

    captions
}
