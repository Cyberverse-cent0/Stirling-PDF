//! Test-only helpers: minimal PDFs built with `lopdf`, run through the real
//! endpoint work functions, and read back with `Document::load_mem`.

use lopdf::{dictionary, Dictionary, Document, Object, ObjectId, Stream};

use crate::pdfdoc;

/// A one-page document skeleton — catalog → pages → page — for tests to
/// hang dictionaries, streams, and annotations off. `bytes` serialises
/// through `pdfdoc::save`, the same path real uploads take.
pub struct Pdf {
    doc: Document,
    pub catalog: ObjectId,
    #[allow(dead_code)]
    pub pages: ObjectId,
    pub page: ObjectId,
}

impl Pdf {
    pub fn new() -> Self {
        let mut doc = Document::with_version("1.7");
        let page = doc.new_object_id();
        let pages = doc.add_object(dictionary! {
            "Type" => "Pages",
            "Kids" => vec![Object::Reference(page)],
            "Count" => 1,
        });
        let catalog = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => Object::Reference(pages),
        });
        doc.objects.insert(
            page,
            Object::Dictionary(dictionary! {
                "Type" => "Page",
                "Parent" => Object::Reference(pages),
                "MediaBox" => Object::Array(vec![0.into(), 0.into(), 612.into(), 792.into()]),
            }),
        );
        doc.trailer.set("Root", Object::Reference(catalog));
        Self {
            doc,
            catalog,
            pages,
            page,
        }
    }

    /// Adds a second (or later) page to the document's page tree.
    /// `media` is `[llx, lly, urx, ury]`; `extra` entries are merged into
    /// the page dictionary (Rotate, Resources, Annots, …).
    pub fn add_page(&mut self, media: [i64; 4], extra: Dictionary) -> ObjectId {
        let mut page = dictionary! {
            "Type" => "Page",
            "Parent" => Object::Reference(self.pages),
            "MediaBox" => Object::Array(media.iter().map(|v| Object::Integer(*v)).collect()),
        };
        for (key, value) in extra {
            page.set(key, value);
        }
        let id = self.doc.add_object(Object::Dictionary(page));
        let mut kids = match self.doc.get_dictionary(self.pages).unwrap().get(b"Kids") {
            Ok(Object::Array(kids)) => kids.clone(),
            _ => Vec::new(),
        };
        kids.push(Object::Reference(id));
        let mut pages = self.dict(self.pages);
        let count = kids.len() as i64;
        pages.set("Kids", Object::Array(kids));
        pages.set("Count", count);
        self.put_dict(self.pages, pages);
        id
    }

    /// Adds an indirect object, returning its id.
    pub fn add(&mut self, object: impl Into<Object>) -> ObjectId {
        self.doc.add_object(object)
    }

    /// Adds a stream object, returning its id.
    pub fn add_stream(&mut self, dict: Dictionary, content: &[u8]) -> ObjectId {
        self.add(Stream::new(dict, content.to_vec()))
    }

    #[allow(dead_code)]
    pub fn object(&self, id: ObjectId) -> &Object {
        self.doc.get_object(id).unwrap()
    }

    pub fn dict(&self, id: ObjectId) -> Dictionary {
        self.doc.get_dictionary(id).unwrap().clone()
    }

    /// Replaces an indirect object's dictionary, like the endpoints' write-backs.
    pub fn put_dict(&mut self, id: ObjectId, dict: Dictionary) {
        self.doc.objects.insert(id, Object::Dictionary(dict));
    }

    pub fn edit_catalog(&mut self, f: impl FnOnce(&mut Dictionary)) {
        let mut catalog = self.dict(self.catalog);
        f(&mut catalog);
        self.put_dict(self.catalog, catalog);
    }

    pub fn edit_page(&mut self, f: impl FnOnce(&mut Dictionary)) {
        let mut page = self.dict(self.page);
        f(&mut page);
        self.put_dict(self.page, page);
    }

    pub fn set_info(&mut self, info: Dictionary) {
        self.doc.trailer.set("Info", Object::Dictionary(info));
    }

    /// The document back as upload bytes.
    pub fn bytes(&mut self) -> Vec<u8> {
        pdfdoc::save(&mut self.doc).unwrap()
    }
}

/// Minimal image XObject stream.
pub fn image_xobject() -> Dictionary {
    dictionary! {
        "Type" => "XObject",
        "Subtype" => "Image",
        "Width" => 1,
        "Height" => 1,
        "ColorSpace" => "DeviceRGB",
        "BitsPerComponent" => 8,
    }
}

/// Reads an endpoint's PDF response body back into a `Document`.
pub async fn response_document(response: axum::response::Response) -> Document {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    Document::load_mem(&bytes).unwrap()
}

#[cfg(test)]
mod fixture_tests {
    use super::*;
    use lopdf::dictionary;
    use std::path::PathBuf;

    fn fixtures_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
    }

    /// Writes the sample PDFs the shell smoke tests POST to the server.
    /// Generated, not checked in — rerun `cargo test` to refresh them.
    #[test]
    fn writes_sample_fixtures() {
        let dir = fixtures_dir();
        std::fs::create_dir_all(&dir).unwrap();

        let mut basic = Pdf::new();
        std::fs::write(dir.join("basic.pdf"), basic.bytes()).unwrap();

        let mut images = Pdf::new();
        let im = images.add_stream(image_xobject(), &[255, 0, 0]);
        images.edit_page(|page| {
            page.set(
                "Resources",
                dictionary! {
                    "XObject" => dictionary! {
                        "Im1" => Object::Reference(im),
                    },
                },
            );
        });
        std::fs::write(dir.join("images.pdf"), images.bytes()).unwrap();

        let mut san = Pdf::new();
        let xmp = san.add_stream(
            dictionary! { "Type" => "Metadata", "Subtype" => "XML" },
            b"<x:xmpmeta/>",
        );
        san.set_info(dictionary! {
            "Title" => Object::string_literal("fixture"),
        });
        san.edit_catalog(|catalog| {
            catalog.set(
                "OpenAction",
                Object::Dictionary(dictionary! {
                    "S" => "JavaScript",
                    "JS" => Object::string_literal("app.alert(1)"),
                }),
            );
            catalog.set("Metadata", Object::Reference(xmp));
        });
        std::fs::write(dir.join("sanitize-target.pdf"), san.bytes()).unwrap();
    }
}
