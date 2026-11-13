use std::collections::BTreeMap;

/// Apply the saved query variables in the same order for every frontend.
pub fn substitute_active_variables(query: &str, variables: &BTreeMap<String, String>) -> String {
    variables
        .iter()
        .fold(query.to_string(), |query, (name, value)| {
            query.replace(&format!("%%{name}%%"), value)
        })
}
