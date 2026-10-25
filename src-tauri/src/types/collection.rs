use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComicCollection {
    pub id: i64,
    pub title: String,
    pub episodes: Vec<ComicEpisode>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComicEpisode {
    pub id: i64,
    pub title: String,
    pub url: String,
}
