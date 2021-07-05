use bytestring::ByteString;
use oso::PolarClass;

#[derive(Clone, PolarClass)]
struct Client {
    #[polar(attribute)]
    addr: Option<String>,
    #[polar(attribute)]
    username: Option<String>,
}
