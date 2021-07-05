use std::path::Path;

use anyhow::Result;
use oso::{Oso, PolarClass};
use service::{RemoteAddr, TopicFilter};

#[derive(Clone, PolarClass)]
pub struct OsoRemoteAddr(pub RemoteAddr);

#[derive(Clone, PolarClass)]
pub struct OsoClient {
    #[polar(attribute)]
    pub addr: OsoRemoteAddr,
    #[polar(attribute)]
    pub username: Option<String>,
}

#[derive(Clone, PolarClass)]
pub struct OsoTopic(pub String);

#[derive(Clone, PolarClass)]
pub struct OsoFilter(Option<TopicFilter>);

pub fn create_oso(path: &Path) -> Result<Oso> {
    let mut oso = Oso::new();
    oso.load_file(path)?;

    oso.register_class(OsoRemoteAddr::get_polar_class())?;
    oso.register_class(OsoClient::get_polar_class())?;
    oso.register_class(
        OsoTopic::get_polar_class_builder()
            .add_method("matches", |topic: &OsoTopic, filter: OsoFilter| {
                if let Some(filter) = &filter.0 {
                    filter.matches(&topic.0)
                } else {
                    false
                }
            })
            .build(),
    )?;
    oso.register_class(
        OsoFilter::get_polar_class_builder()
            .set_constructor(|filter: String| OsoFilter(TopicFilter::try_new(&filter)))
            .build(),
    )?;

    Ok(oso)
}
