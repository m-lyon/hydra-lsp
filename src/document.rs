use dashmap::DashMap;
use tower_lsp::lsp_types::Url;

#[derive(Debug)]
pub struct Document {
    pub content: String,
    pub version: i32,
}

impl Document {
    pub fn new(content: String, version: i32) -> Self {
        Self { content, version }
    }
}

#[derive(Debug, Default)]
pub struct DocumentStore {
    documents: DashMap<Url, Document>,
}

impl DocumentStore {
    pub fn new() -> Self {
        Self {
            documents: DashMap::new(),
        }
    }

    pub fn insert(&self, uri: Url, content: String, version: i32) {
        self.documents.insert(uri, Document::new(content, version));
    }

    pub fn update(&self, uri: Url, content: String, version: i32) {
        if let Some(mut doc) = self.documents.get_mut(&uri) {
            doc.content = content;
            doc.version = version;
        }
    }

    pub fn get(&self, uri: &Url) -> Option<Document> {
        self.documents
            .get(uri)
            .map(|doc| Document::new(doc.content.clone(), doc.version))
    }

    pub fn remove(&self, uri: &Url) {
        self.documents.remove(uri);
    }
}
