// An HTML document to send as a response body
pub struct Html(String);

impl Html {
    pub fn new(html: impl Into<String>) -> Self {
        Html(html.into())
    }

    pub fn into_string(self) -> String {
        self.0
    }
}
