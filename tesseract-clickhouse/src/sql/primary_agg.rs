use itertools::join;

use super::aggregator::{
    agg_sql_string_pass_1,
    agg_sql_string_pass_2,
    agg_sql_string_select_mea,
};
use super::cuts::cut_sql_string;
use super::{
    TableSql,
    CutSql,
    DrilldownSql,
    MeasureSql,
    HiddenDrilldownSql,
    dim_subquery,
};


/// Error checking is done before this point. This string formatter
/// accepts any input
pub fn primary_agg(
    table: &TableSql,
    cuts: &[CutSql],
    drills: &[DrilldownSql],
    meas: &[MeasureSql],
    hidden_drills: Option<&[HiddenDrilldownSql]>,
    ) -> (String, String)
{
    // Before first section, need to separate out inline dims.
    // These are the ones that have the same dim table as fact table.
    //
    // First section, get drill/cut combos lined up.
    //
    // First "zip" drill and cut into DimSubquery
    // - pop drill, attempt to match with cut (remove cut if used (sounds sketchy, but could swap
    // with empty struct))
    // - go through remaining cuts (if had swapped empty struct, go through ones that aren't empty)
    //
    // Then, the order is:
    // - any dimension that has the same primary key as the
    // - doesn't matter
    //
    // So just swap the primary key DimSubquery to the head

    let mut ext_drills: Vec<_> = drills.iter()
        .filter(|d| {
            if d.inline_table.is_some() {
                true
            } else {
                d.table.name != table.name
            }
        })
        .collect();

    let ext_cuts: Vec<_> = cuts.iter()
        .filter(|c| c.table.name != table.name || c.inline_table.is_some())
        .collect();
    let ext_cuts_for_inline = ext_cuts.clone();

    let inline_drills: Vec<_> = drills.iter()
        .filter(|d| {
            if d.inline_table.is_some() {
                false
            } else {
                d.table.name == table.name
            }
        })
        .collect();

    let inline_cuts: Vec<_> = cuts.iter()
        .filter(|c| c.table.name == table.name && !c.inline_table.is_some())
        .collect();

    let mut dim_subqueries = vec![];

    // external drill and cuts section

    while let Some(drill) = ext_drills.pop() {
        // TODO can this be removed?
//        if let Some(idx) = ext_cuts.iter().position(|c| c.table == drill.table) { // TODO bug here, can't just match on table
//            let cut = ext_cuts.swap_remove(idx);
//
//            dim_subqueries.push(
//                dim_subquery(Some(drill),Some(cut))
//            );
//        } else {
            dim_subqueries.push(
                dim_subquery(Some(drill), None)
            );
//        }
    }
    // TODO can this be removed entirely?
//
//    for cut in &ext_cuts {
//        dim_subqueries.push(
//            dim_subquery(None, Some(cut))
//        );
//    }

    if let Some(ref primary_key) = table.primary_key {
        if let Some(idx) = dim_subqueries.iter().position(|d| d.foreign_key == *primary_key) {
            dim_subqueries.swap(0, idx);
        }
    }

    // Now set up fact table query
    // Group by is hardcoded in because there's an assumption that at least one
    // dim exists
    //
    // This is also the section where inline dims and cuts get put
    //
    // Note new feature: hidden drilldowns are added here; they are added to the
    // select here, and the group by; but do not have the columns projected upstream.

    let mea_cols = meas
        .iter()
        .enumerate()
        .map(|(i, m)| {
            // should return "m.aggregator({m.col}) as m{i}" for simple cases
            agg_sql_string_pass_1(&m.column, &m.aggregator, i)
        }
        );
    let mea_cols = join(mea_cols, ", ");

    let inline_dim_cols = inline_drills.iter().map(|d| d.col_alias_string());
    let inline_dim_aliass = inline_drills.iter().map(|d| d.col_alias_only_string());

    let dim_idx_cols = dim_subqueries.iter().map(|d| d.foreign_key.clone());

    let all_fact_dim_cols = join(inline_dim_cols.chain(dim_idx_cols.clone()), ", ");
    let all_fact_dim_aliass = join(inline_dim_aliass.chain(dim_idx_cols), ", ");

    // TODO remove allocation
    let hidden_drills = hidden_drills.map(|ds| ds.to_vec()).unwrap_or(vec![]);
    let hidden_dim_cols = join(hidden_drills.iter().map(|d| d.drilldown_sql.col_alias_string()), ", ");

    let mut fact_sql = format!("select {}", all_fact_dim_cols);

    // done separately so that it isn't projected up the subqueries
    if !hidden_drills.is_empty() {
        fact_sql.push_str(&format!(", {}", hidden_dim_cols));
    }

    fact_sql.push_str(&format!(", {} from {}", mea_cols, table.name));

    if (inline_cuts.len() > 0) || (ext_cuts_for_inline.len() > 0) {
        let inline_cut_clause = inline_cuts
            .iter()
            .map(|c| cut_sql_string(&c));

        let ext_cut_clause = ext_cuts_for_inline
            .iter()
            .map(|c| {
                let cut_table = match &c.inline_table {
                    Some(it) => {
                        let inline_table_sql = it.sql_string();
                        format!("({}) as {}", inline_table_sql, c.table.full_name())
                    },
                    None => c.table.full_name()
                };

                if c.members.is_empty() {
                    // this case is for default hierarchy
                    // in multiple hierarchies
                    format!("{} in (select {} from {})",
                        c.foreign_key,
                        c.primary_key,
                        cut_table,
                    )
                } else {
                    format!("{} in (select {} from {} where {})",
                        c.foreign_key,
                        c.primary_key,
                        cut_table,
                        cut_sql_string(&c),
                    )
                }
            });

        let cut_clause = join(inline_cut_clause.chain(ext_cut_clause), "and ");

        fact_sql.push_str(&format!(" where {}", cut_clause));
    }

    fact_sql.push_str(&format!(" group by {}", all_fact_dim_aliass));

    // done separately so that it isn't projected up the subqueries
    if !hidden_drills.is_empty() {
        fact_sql.push_str(&format!(", {}", hidden_dim_cols));
    }

    // Now second half, feed DimSubquery into the multiple joins with fact table
    // TODO allow for differently named cols to be joined on. (using an alias for as)

    let mut sub_queries = fact_sql;

    // initialize current dim cols with inline drills and idx cols (all dim cols)
    let mut current_dim_cols = vec![all_fact_dim_aliass];

    // Create sql string for the measures that are carried up from the
    // fact table query
    let select_mea_cols = meas
        .iter()
        .enumerate()
        .map(|(i, m)| {
            // should return "m{i}" for simple cases
            agg_sql_string_select_mea(&m.aggregator, i)
        });
    let select_mea_cols = join(select_mea_cols, ", ");

    for dim_subquery in dim_subqueries {
        // This section needed to accumulate the dim cols that are being selected over
        // the recursive joins.
        if let Some(cols) = dim_subquery.dim_cols {
            current_dim_cols.push(cols);
        }

        let sub_queries_dim_cols = if !current_dim_cols.is_empty() {
            format!("{}, ", join(current_dim_cols.iter(), ", "))
        } else {
            "".to_owned()
        };

        // Now construct subquery
        sub_queries = format!("select {}{} from ({}) all inner join ({}) using {}",
            sub_queries_dim_cols,
            select_mea_cols,
            dim_subquery.sql,
            sub_queries,
            dim_subquery.foreign_key
        );
    }

    // Finally, wrap with final agg and result
    let final_drill_cols = drills.iter().map(|drill| drill.col_alias_only_string());
    let final_drill_cols = join(final_drill_cols, ", ");

    let final_mea_cols = meas.iter().enumerate().map(|(i, mea)| {
            // should return "m.aggregator(m{i}) as final_m{i}" for simple cases
            agg_sql_string_pass_2(&mea.aggregator, i)
        });
    let final_mea_cols = join(final_mea_cols, ", ");

    // This is the final result of the groupings.
    let final_sql = format!("select {}, {} from ({}) group by {}",
        final_drill_cols,
        final_mea_cols,
        sub_queries,
        final_drill_cols,
    );

    (final_sql, final_drill_cols)
}
