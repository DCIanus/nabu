use crate::{
    config::{cache_duration, serve_mode, ServeMode},
    database::get_connection,
    errors::QSParseError,
    feed_generator::FeedGenerator,
    responses,
    source::Source,
    utils::{now, NabuResult},
};
use actix_web::{HttpRequest, HttpResponse};
use atom_syndication::{Feed, FixedDateTime, Generator};
use log::error;
use serde_json::Value;

#[derive(Clone)]
pub struct FeedWorker {
    pub prefix: String,
    pub path: String,
    pub clean_query_string: fn(&str) -> NabuResult<Value>,
    pub update_by_value: fn(Value) -> NabuResult<Feed>,
}

impl FeedWorker {
    pub fn new<T: FeedGenerator>(source: &Source, _: T) -> Self {
        let prefix = source
            .prefix
            .split('/')
            .filter(|x| !x.is_empty())
            .collect::<Vec<&str>>()
            .join("/");
        let path = T::PATH
            .split('/')
            .filter(|x| !x.is_empty())
            .collect::<Vec<&str>>()
            .join("/");
        FeedWorker {
            prefix,
            path,
            clean_query_string: T::clean_query_string,
            update_by_value: T::update_by_value,
        }
    }

    pub fn get_cache(&self, info: &Value) -> NabuResult<Option<Feed>> {
        let query_result = get_connection()?
            .query(r"SELECT updated_time, content FROM fetch_cache WHERE prefix=$1 AND path=$2 AND info@> $3 AND info<@ $3 limit 1", &[
                &self.prefix, &self.path, info
            ])?;
        if query_result.is_empty() {
            return Ok(None);
        }
        let row = query_result.get(0);

        let updated_time: FixedDateTime = row.get(0);
        let content: Value = row.get(1);
        let feed = ::serde_json::from_value::<Feed>(content)?;

        if now().signed_duration_since(updated_time).to_std()? > cache_duration() {
            Ok(None)
        } else {
            Ok(Some(feed))
        }
    }

    pub fn put_cache(&self, info: &Value, feed: &Feed) -> NabuResult<()> {
        let content = ::serde_json::to_value(feed)?;
        get_connection()?.execute(
            r#"INSERT INTO fetch_cache(prefix, path, info, content)
                            VALUES ($1, $2, $3, $4)
                            ON CONFLICT ON CONSTRAINT logic_unique_key DO UPDATE
                                SET content=$4"#,
            &[&self.prefix, &self.path, info, &content],
        )?;
        Ok(())
    }

    pub fn into_actix_web_handler(self) -> impl Fn(&HttpRequest) -> HttpResponse {
        move |request: &HttpRequest| {
            let query_string = request.query_string();

            let value = match (self.clean_query_string)(query_string) {
                Ok(value) => value,
                Err(error) => match error.downcast::<QSParseError>() {
                    Ok(parse_error) => {
                        error!("{:?}", parse_error);
                        return responses::parse_query_string_failed();
                    }
                    Err(unexpected_error) => {
                        error!("{:?}", unexpected_error);
                        return responses::unexpected_error();
                    }
                },
            };

            if serve_mode() != ServeMode::Dev {
                // Logic for get cache
                match self.get_cache(&value) {
                    Ok(cache_option) => {
                        if let Some(cache) = cache_option {
                            return responses::cache_hit(cache.to_string());
                        }
                    }
                    Err(unexpected_error) => {
                        error!("{:?}", unexpected_error);
                        return responses::unexpected_error();
                    }
                }
            }

            match (self.update_by_value)(value.clone()) {
                Ok(mut feed) => {
                    feed.set_generator(Some(Generator {
                        value: "Nabu".to_string(),
                        uri: Some("https://github.com/DCjanus/nabu".to_string()),
                        version: None,
                    }));

                    if let Err(put_cache_error) = self.put_cache(&value, &feed) {
                        error!("{:?}", put_cache_error);
                    }

                    responses::feed_created(feed.to_string())
                }
                Err(update_error) => {
                    error!("{:?}", update_error);
                    responses::unexpected_error()
                }
            }
        }
    }
}
