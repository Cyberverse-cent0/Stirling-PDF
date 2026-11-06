//! AcroForm field model, the Rust counterpart of jpdfium's
//! `doc/FormField` + `doc/FormFieldType`. Values match PDFium's
//! `FPDF_FORMFIELD_*` codes so call sites port one-to-one.

/// Widget rectangle in default user space (y up), like jpdfium's `Rect`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Field types as reported by PDFium's `FPDFAnnot_GetFormFieldType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormFieldType {
    Unknown,
    Pushbutton,
    Checkbox,
    Radio,
    Combobox,
    Listbox,
    Text,
    Signature,
}

/// A form field read from a widget annotation — jpdfium `FormField`.
#[derive(Debug, Clone)]
pub struct FormField {
    pub page_index: usize,
    pub name: String,
    pub field_type: FormFieldType,
    pub value: Option<String>,
    pub export_value: Option<String>,
    pub checked: bool,
    pub read_only: bool,
    pub required: bool,
    pub tooltip: String,
    pub options: Vec<String>,
    /// Indices into `options` selected in a choice field.
    pub selected_option_indices: Vec<usize>,
    pub rect: Option<Rect>,
}
