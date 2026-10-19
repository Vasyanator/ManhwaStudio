/*
File: crates/ms-ai-api/src/image_edit/multipart.rs

Purpose:
Compatibility re-export of the shared `multipart/form-data` builder, which lives in
`crate::http::multipart` (its owner and its tests). The image-edit adapters keep importing it
from here (`image_edit::multipart::MultipartForm`); `MultipartForm::finish` fails with
`HttpError::RequestBuild`, which `?` turns into `ImageEditError::RequestBuild`.

Key structures:
- MultipartForm, MultipartBody (re-exported)
*/

pub use crate::http::{MultipartBody, MultipartForm};
