use comemo::Prehashed;
use fast_image_resize as fr;
use render::format_diagnostic;
use std::{
    cell::{OnceCell, RefCell, RefMut},
    collections::HashMap,
    path::{Path, PathBuf},
};
use typst::{
    diag::{EcoString, FileError, FileResult, PackageError, PackageResult},
    eval::{Bytes, Datetime, Library, Tracer},
    font::{Font, FontBook},
    syntax::Source,
    syntax::{FileId, PackageSpec},
    util::PathExt,
    World,
};
use wasm_bindgen::prelude::*;
use web_sys::ImageData;

mod file_entry;
mod render;

use crate::file_entry::FileEntry;

/// A world that provides access to the operating system.
#[wasm_bindgen]
pub struct SystemWorld {
    /// The root relative to which absolute paths are resolved.
    root: PathBuf,
    /// The input source.
    main: FileId,
    /// Typst's standard library.
    library: Prehashed<Library>,
    /// Metadata about discovered fonts.
    book: Prehashed<FontBook>,
    /// Storage of fonts
    fonts: Vec<Font>,

    files: RefCell<HashMap<FileId, FileEntry>>,
    /// The current date if requested. This is stored here to ensure it is
    /// always the same within one compilation. Reset between compilations.
    today: OnceCell<Option<Datetime>>,

    packages: RefCell<HashMap<PackageSpec, PackageResult<PathBuf>>>,

    resizer: fr::Resizer,

    js_request_data: js_sys::Function,
}

#[wasm_bindgen]
impl SystemWorld {
    #[wasm_bindgen(constructor)]
    pub fn new(root: String, js_read_file: &js_sys::Function) -> SystemWorld {
        console_error_panic_hook::set_once();

        let (book, fonts) = SystemWorld::start_embedded_fonts();

        Self {
            root: PathBuf::from(root),
            main: FileId::detached(),
            library: Prehashed::new(typst_library::build()),
            book: Prehashed::new(book),
            fonts,
            files: RefCell::default(),
            today: OnceCell::new(),
            packages: RefCell::default(),
            resizer: fr::Resizer::default(),
            js_request_data: js_read_file.clone(),
        }
    }

    pub fn compile(
        &mut self,
        // command: CompileCommand,
        text: String,
        path: String,
        pixel_per_pt: f32,
        fill: String,
        size: u32,
        display: bool,
    ) -> Result<ImageData, JsValue> {
        self.reset();

        self.main = FileId::new(None, &PathBuf::from(&path));
        self.files.borrow_mut().insert(
            self.main,
            FileEntry::new(self.main, text), //     bytes: OnceCell::new(),
                                             //     source: Source::new(self.main, text),
                                             // },
        );
        let mut tracer = Tracer::default();
        match typst::compile(self, &mut tracer) {
            Ok(document) => render::to_image(
                &mut self.resizer,
                document,
                fill,
                pixel_per_pt,
                size,
                display,
            ),
            Err(errors) => Err(format_diagnostic(self.files.borrow(), &errors).into()),
        }
    }

    pub fn add_font(&mut self, data: Vec<u8>) {
        let buffer = Bytes::from(data);
        let mut font_infos = Vec::new();
        for font in Font::iter(buffer) {
            font_infos.push(font.info().clone());
            self.fonts.push(font)
        }
        if font_infos.len() > 0 {
            self.book.update(|b| {
                for info in font_infos {
                    b.push(info)
                }
            });
        }
    }
}

impl World for SystemWorld {
    fn library(&self) -> &Prehashed<Library> {
        &self.library
    }

    fn book(&self) -> &Prehashed<FontBook> {
        &self.book
    }

    fn main(&self) -> Source {
        self.source(self.main).unwrap()
    }

    fn source(&self, id: FileId) -> FileResult<Source> {
        Ok(self.file_entry(id)?.source())
    }

    fn file(&self, id: FileId) -> FileResult<Bytes> {
        Ok(self.file_entry(id)?.bytes())
    }

    fn font(&self, index: usize) -> Option<Font> {
        Some(self.fonts[index].clone())
    }

    fn today(&self, _: Option<i64>) -> Option<Datetime> {
        None
    }
}

impl SystemWorld {
    fn reset(&mut self) {
        self.files.borrow_mut().clear();
        self.today.take();
    }

    fn read_file(&self, path: &Path) -> FileResult<String> {
        let f = |_e: JsValue| FileError::Other;
        Ok(self
            .js_request_data
            .call1(&JsValue::NULL, &path.to_str().unwrap().into())
            .map_err(f)?
            .as_string()
            .unwrap())
    }

    fn prepare_package(&self, spec: &PackageSpec) -> PackageResult<PathBuf> {
        let f = |e: JsValue| {
            if let Some(num) = e.as_f64() {
                if num == -2.0 {
                    return PackageError::NotFound(spec.clone());
                }
            }
            PackageError::Other
        };
        self.packages
            .borrow_mut()
            .entry(spec.clone())
            .or_insert_with(|| {
                Ok(self
                    .js_request_data
                    .call1(
                        &JsValue::NULL,
                        &format!("@{}/{}-{}", spec.namespace, spec.name, spec.version).into(),
                    )
                    .map_err(f)?
                    .as_string()
                    .unwrap()
                    .into())
            })
            .clone()
    }

    fn file_entry(&self, id: FileId) -> FileResult<RefMut<FileEntry>> {
        if let Ok(file) = RefMut::filter_map(self.files.borrow_mut(), |files| files.get_mut(&id)) {
            return Ok(file);
        }

        let path = match id.package() {
            Some(spec) => self.prepare_package(spec)?,
            None => self.root.clone(),
        }
        .join_rooted(id.path())
        .ok_or(FileError::AccessDenied)?;
        let text = self.read_file(&path)?;
        Ok(RefMut::map(self.files.borrow_mut(), |files| {
            return files.entry(id).or_insert(FileEntry::new(id, text));
        }))
    }

    fn start_embedded_fonts() -> (FontBook, Vec<Font>) {
        let mut book = FontBook::new();
        let mut fonts = Vec::new();

        let mut process = |bytes: &'static [u8]| {
            let buffer = Bytes::from_static(bytes);
            for font in Font::iter(buffer) {
                book.push(font.info().clone());
                fonts.push(font);
            }
        };

        macro_rules! add {
            ($filename:literal) => {
                process(include_bytes!(concat!("../assets/fonts/", $filename)));
            };
        }

        // Embed default fonts.
        add!("LinLibertine_R.ttf");
        add!("LinLibertine_RB.ttf");
        add!("LinLibertine_RBI.ttf");
        add!("LinLibertine_RI.ttf");
        add!("NewCMMath-Book.otf");
        add!("NewCMMath-Regular.otf");
        add!("DejaVuSansMono.ttf");
        add!("DejaVuSansMono-Bold.ttf");
        add!("DejaVuSansMono-Oblique.ttf");
        add!("DejaVuSansMono-BoldOblique.ttf");

        return (book, fonts);
    }
}
