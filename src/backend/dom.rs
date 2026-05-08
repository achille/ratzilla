use std::{
    cell::RefCell,
    io::{Error as IoError, Result as IoResult},
    rc::Rc,
};

use ratatui::{
    backend::WindowSize,
    buffer::Cell,
    layout::{Position, Size},
    prelude::{backend::ClearType, Backend},
    style::Modifier,
};
use web_sys::{wasm_bindgen::JsCast, window, Document, Element};

use unicode_width::UnicodeWidthStr;

use crate::{
    backend::{
        cell_sized::CellSized,
        event_callback::{
            create_mouse_event, create_wheel_event, EventCallback, MouseConfig, KEY_EVENT_TYPES,
            MOUSE_EVENT_TYPES, WHEEL_EVENT_TYPES,
        },
        utils::*,
    },
    error::Error,
    event::{KeyEvent, MouseEvent},
    render::WebEventHandler,
    CursorShape,
};

/// Default cell size used as a fallback when measurement fails.
const DEFAULT_CELL_SIZE: (f64, f64) = (10.0, 20.0);
const HYPERLINK_MODIFIER: Modifier = Modifier::SLOW_BLINK;
const HYPERLINK_SELECTOR: &str = "a[data-ratzilla-hyperlink=\"true\"]";
const HYPERLINK_CLICK_EVENT_TYPES: &[&str] = &["click"];
const HYPERLINK_STYLE: &str = "color: #268bd2; text-decoration: underline; cursor: pointer;";

/// Options for the [`DomBackend`].
#[derive(Debug, Default)]
pub struct DomBackendOptions {
    /// The element ID.
    grid_id: Option<String>,
    /// The cursor shape.
    cursor_shape: CursorShape,
}

impl DomBackendOptions {
    /// Constructs a new [`DomBackendOptions`].
    pub fn new(grid_id: Option<String>, cursor_shape: CursorShape) -> Self {
        Self {
            grid_id,
            cursor_shape,
        }
    }

    /// Returns the grid ID.
    ///
    /// - If the grid ID is not set, it returns `"grid"`.
    /// - If the grid ID is set, it returns the grid ID suffixed with
    ///     `"_ratzilla_grid"`.
    pub fn grid_id(&self) -> String {
        match &self.grid_id {
            Some(id) => format!("{id}_ratzilla_grid"),
            None => "grid".to_string(),
        }
    }

    /// Returns the [`CursorShape`].
    pub fn cursor_shape(&self) -> &CursorShape {
        &self.cursor_shape
    }
}

/// DOM backend.
///
/// This backend uses the DOM to render the content to the screen.
///
/// In other words, it transforms the [`Cell`]s into `<span>`s which are then
/// appended to a `<pre>` element.
pub struct DomBackend {
    /// Whether the backend has been initialized.
    initialized: Rc<RefCell<bool>>,
    /// Cells.
    cells: Vec<Element>,
    /// Cell state used to rebuild rows when contiguous hyperlinks change.
    cell_buffer: Vec<Cell>,
    /// Per-frame row flags reused to avoid draw-loop allocations.
    rows_need_rebuild: Vec<bool>,
    /// Per-frame row touch flags reused to avoid draw-loop allocations.
    rows_touched: Vec<bool>,
    /// Changed cell indexes for the plain-row fast path.
    changed_cells: Vec<usize>,
    /// Row elements.
    rows: Vec<Element>,
    /// Grid element.
    grid: Element,
    /// The parent of the grid element.
    grid_parent: Element,
    /// Document.
    document: Document,
    /// Options.
    options: DomBackendOptions,
    /// Cursor position.
    cursor_position: Option<Position>,
    /// Last Cursor position.
    last_cursor_position: Option<Position>,
    /// Buffer size to pass to [`ratatui::Terminal`]
    size: Size,
    /// Measured cell dimensions in pixels (width, height).
    cell_size: (f64, f64),
    /// Resize event callback handler.
    _resize_callback: EventCallback<web_sys::Event>,
    /// Mouse event callback handler.
    mouse_callback: Option<DomMouseCallbackState>,
    /// Wheel event callback handler.
    wheel_callback: Option<EventCallback<web_sys::WheelEvent>>,
    /// Hyperlink click callback handler.
    hyperlink_click_callback: Option<EventCallback<web_sys::MouseEvent>>,
    /// Key event callback handler.
    key_callback: Option<EventCallback<web_sys::KeyboardEvent>>,
}

/// Type alias for mouse event callback state.
type DomMouseCallbackState = EventCallback<web_sys::MouseEvent>;

impl std::fmt::Debug for DomBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DomBackend")
            .field("initialized", &self.initialized)
            .field("cells", &format!("[{} cells]", self.cells.len()))
            .field(
                "cell_buffer",
                &format!("[{} cells]", self.cell_buffer.len()),
            )
            .field(
                "rows_need_rebuild",
                &format!("[{} rows]", self.rows_need_rebuild.len()),
            )
            .field(
                "rows_touched",
                &format!("[{} rows]", self.rows_touched.len()),
            )
            .field(
                "changed_cells",
                &format!("[{} cells]", self.changed_cells.len()),
            )
            .field("size", &self.size)
            .field("cell_size", &self.cell_size)
            .field("cursor_position", &self.cursor_position)
            .field("resize_callback", &"...")
            .field("mouse_callback", &self.mouse_callback.is_some())
            .field("wheel_callback", &self.wheel_callback.is_some())
            .field(
                "hyperlink_click_callback",
                &self.hyperlink_click_callback.is_some(),
            )
            .field("key_callback", &self.key_callback.is_some())
            .finish()
    }
}

impl DomBackend {
    /// Constructs a new [`DomBackend`].
    pub fn new() -> Result<Self, Error> {
        Self::new_with_options(DomBackendOptions::default())
    }

    /// Constructs a new [`DomBackend`] and uses the given element ID for the grid.
    pub fn new_by_id(id: &str) -> Result<Self, Error> {
        Self::new_with_options(DomBackendOptions::new(
            Some(id.to_string()),
            CursorShape::default(),
        ))
    }

    /// Set the [`CursorShape`].
    pub fn set_cursor_shape(mut self, shape: CursorShape) -> Self {
        self.options.cursor_shape = shape;
        self
    }

    /// Constructs a new [`DomBackend`] with the given options.
    pub fn new_with_options(options: DomBackendOptions) -> Result<Self, Error> {
        let window = window().ok_or(Error::UnableToRetrieveWindow)?;
        let document = window.document().ok_or(Error::UnableToRetrieveDocument)?;
        let grid_parent = get_element_by_id_or_body(options.grid_id.as_ref())?;
        let cell_size =
            Self::measure_cell_size(&document, &grid_parent).unwrap_or(DEFAULT_CELL_SIZE);
        let size = Self::calculate_size(&grid_parent, cell_size);

        let initialized = Rc::new(RefCell::new(false));
        let initialized_cb = initialized.clone();
        let resize_callback = EventCallback::new(
            window.clone(),
            Self::RESIZE_EVENT_TYPES,
            move |_: web_sys::Event| {
                initialized_cb.replace(false);
            },
        )?;

        let mut backend = Self {
            initialized,
            cells: vec![],
            cell_buffer: vec![],
            rows_need_rebuild: vec![],
            rows_touched: vec![],
            changed_cells: vec![],
            rows: vec![],
            grid: document.create_element("div")?,
            grid_parent,
            options,
            document,
            cursor_position: None,
            last_cursor_position: None,
            size,
            cell_size,
            _resize_callback: resize_callback,
            mouse_callback: None,
            wheel_callback: None,
            hyperlink_click_callback: None,
            key_callback: None,
        };
        backend.reset_grid()?;
        Ok(backend)
    }

    /// Measures the pixel dimensions of a single terminal cell.
    ///
    /// Creates a temporary `<pre><span>` probe element that inherits the
    /// page's CSS (font-family, font-size, etc.), measures it with
    /// `getBoundingClientRect()`, then removes the probe.
    fn measure_cell_size(document: &Document, parent: &Element) -> Result<(f64, f64), Error> {
        let pre = document.create_element("pre")?;
        pre.set_attribute(
            "style",
            "margin: 0; padding: 0; border: 0; line-height: normal; font-family: inherit;",
        )?;
        let span = document.create_element("span")?;
        span.set_inner_html("\u{2588}");
        span.set_attribute("style", "display: inline-block; width: 1ch;")?;
        pre.append_child(&span)?;
        parent.append_child(&pre)?;

        let rect = span.get_bounding_client_rect();
        let width = rect.width();
        let height = rect.height();

        parent.remove_child(&pre)?;

        if width > 0.0 && height > 0.0 {
            Ok((width, height))
        } else {
            Ok(DEFAULT_CELL_SIZE)
        }
    }

    /// Calculates the grid size in cells based on the parent element's dimensions and cell size.
    fn calculate_size(parent: &Element, cell_size: (f64, f64)) -> Size {
        let rect = parent.get_bounding_client_rect();
        let (parent_w, parent_h) = (rect.width(), rect.height());

        // Fall back to window dimensions if the parent has no size
        // (e.g. empty <body> with no explicit height)
        let (w, h) = if parent_w > 0.0 && parent_h > 0.0 {
            (parent_w, parent_h)
        } else {
            let (ww, wh) = get_raw_window_size();
            (ww as f64, wh as f64)
        };

        Size::new((w / cell_size.0) as u16, (h / cell_size.1) as u16)
    }

    /// Resize event types.
    const RESIZE_EVENT_TYPES: &[&str] = &["resize"];

    /// Reset the grid and clear the cells.
    fn reset_grid(&mut self) -> Result<(), Error> {
        self.grid = self.document.create_element("div")?;
        self.grid.set_attribute("id", &self.options.grid_id())?;
        self.cells.clear();
        self.cell_buffer.clear();
        self.rows_need_rebuild.clear();
        self.rows_touched.clear();
        self.changed_cells.clear();
        self.rows.clear();
        self.hyperlink_click_callback = None;
        Ok(())
    }

    /// Pre-render a blank content to the screen.
    ///
    /// This function is called from [`draw`] once (or after a resize)
    /// to render the right number of cells to the screen.
    fn populate(&mut self) -> Result<(), Error> {
        for _y in 0..self.size.height {
            let mut line_cells: Vec<Element> = Vec::new();
            for _x in 0..self.size.width {
                let cell = Cell::default();
                let span = create_span(&self.document, &cell)?;
                self.cells.push(span.clone());
                self.cell_buffer.push(cell);
                line_cells.push(span);
            }

            // Create a <pre> element for the line
            let pre = self.document.create_element("pre")?;
            let line_height = format!(
                "margin: 0; padding: 0; border: 0; font-family: inherit; height: {}px; line-height: {}px;",
                self.cell_size.1, self.cell_size.1
            );
            pre.set_attribute("style", &line_height)?;
            self.rows.push(pre.clone());

            // Append all elements (spans and anchors) to the <pre>
            for elem in line_cells {
                pre.append_child(&elem)?;
            }

            // Append the <pre> to the grid
            self.grid.append_child(&pre)?;
        }
        Ok(())
    }

    fn install_hyperlink_click_handler(&mut self) -> Result<(), Error> {
        self.hyperlink_click_callback = Some(EventCallback::new(
            self.grid.clone(),
            HYPERLINK_CLICK_EVENT_TYPES,
            move |event: web_sys::MouseEvent| {
                let Some(anchor) = event
                    .target()
                    .and_then(|target| target.dyn_into::<Element>().ok())
                    .and_then(|element| element.closest(HYPERLINK_SELECTOR).ok().flatten())
                else {
                    return;
                };

                let Some(url) = anchor.get_attribute("href") else {
                    return;
                };

                event.prevent_default();
                if let Some(w) = window() {
                    let _ = w.open_with_url_and_target(&url, "_blank");
                }
            },
        )?);

        Ok(())
    }

    fn is_hyperlink_cell(cell: &Cell) -> bool {
        cell.modifier.contains(HYPERLINK_MODIFIER)
    }

    fn hyperlink_cell_style(cell: &Cell) -> String {
        let mut style = get_cell_style_as_css(cell);
        style.push(' ');
        style.push_str(HYPERLINK_STYLE);
        style
    }

    fn set_cell_content(elem: &Element, cell: &Cell, is_hyperlink: bool) -> Result<(), Error> {
        elem.set_text_content(Some(cell.symbol()));
        let style = if is_hyperlink {
            Self::hyperlink_cell_style(cell)
        } else {
            get_cell_style_as_css(cell)
        };
        elem.set_attribute("style", &style)?;
        Ok(())
    }

    fn create_hyperlink_anchor(&self, url: &str) -> Result<Element, Error> {
        let anchor = self.document.create_element("a")?;
        anchor.set_attribute("href", url)?;
        anchor.set_attribute("target", "_blank")?;
        anchor.set_attribute("rel", "noopener")?;
        anchor.set_attribute("data-ratzilla-hyperlink", "true")?;
        anchor.set_attribute("style", HYPERLINK_STYLE)?;
        Ok(anchor)
    }

    fn render_row(&self, y: u16) -> Result<(), Error> {
        let Some(row) = self.rows.get(y as usize) else {
            return Ok(());
        };

        while let Some(child) = row.first_child() {
            row.remove_child(&child)?;
        }

        let width = self.size.width as usize;
        let row_start = y as usize * width;
        let row_end = row_start + width;
        let mut cell_index = row_start;

        while cell_index < row_end {
            let cell = &self.cell_buffer[cell_index];
            if !Self::is_hyperlink_cell(cell) {
                Self::set_cell_content(&self.cells[cell_index], cell, false)?;
                row.append_child(&self.cells[cell_index])?;
                cell_index += 1;
                continue;
            }

            let link_start = cell_index;
            let mut url = String::new();
            while cell_index < row_end && Self::is_hyperlink_cell(&self.cell_buffer[cell_index]) {
                url.push_str(self.cell_buffer[cell_index].symbol());
                cell_index += 1;
            }

            let anchor = self.create_hyperlink_anchor(&url)?;
            for index in link_start..cell_index {
                Self::set_cell_content(&self.cells[index], &self.cell_buffer[index], true)?;
                anchor.append_child(&self.cells[index])?;
            }
            row.append_child(&anchor)?;
        }

        Ok(())
    }
}

impl CellSized for DomBackend {
    fn cell_size_px(&self) -> (f32, f32) {
        let dpr = get_device_pixel_ratio();
        (self.cell_size.0 as f32 * dpr, self.cell_size.1 as f32 * dpr)
    }

    fn cell_size_css_px(&self) -> (f32, f32) {
        (self.cell_size.0 as f32, self.cell_size.1 as f32)
    }
}

impl Backend for DomBackend {
    type Error = IoError;

    /// Draw the new content to the screen.
    ///
    /// This function is called in the [`ratatui::Terminal::flush`] function.
    /// This function recreate the DOM structure when it gets a resize event.
    fn draw<'a, I>(&mut self, content: I) -> IoResult<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        if !*self.initialized.borrow() {
            self.initialized.replace(true);

            // Clear cursor position to avoid modifying css style of a non-existent cell
            self.cursor_position = None;
            self.last_cursor_position = None;

            // Only runs on resize event.
            if self
                .document
                .get_element_by_id(&self.options.grid_id())
                .is_some()
            {
                self.grid_parent.set_inner_html("");
                self.reset_grid()?;

                // re-measure cell size and update grid dimensions
                self.cell_size = Self::measure_cell_size(&self.document, &self.grid_parent)
                    .unwrap_or(DEFAULT_CELL_SIZE);
                self.size = Self::calculate_size(&self.grid_parent, self.cell_size);
            }

            self.grid_parent
                .append_child(&self.grid)
                .map_err(Error::from)?;
            self.populate()?;
            self.install_hyperlink_click_handler()?;

            // Auto-focus the grid so keyboard events are captured immediately.
            if let Some(html_el) = self.grid.dyn_ref::<web_sys::HtmlElement>() {
                let _ = html_el.focus();
            }
        }

        // Track which rows have hyperlink structure changes (need full DOM
        // rebuild) vs plain rows (can update changed spans in-place).
        let row_count = self.size.height as usize;
        if self.rows_need_rebuild.len() != row_count {
            self.rows_need_rebuild.resize(row_count, false);
        } else {
            self.rows_need_rebuild.fill(false);
        }
        if self.rows_touched.len() != row_count {
            self.rows_touched.resize(row_count, false);
        } else {
            self.rows_touched.fill(false);
        }
        self.changed_cells.clear();

        for (x, y, cell) in content {
            let cell_position = (y * self.size.width + x) as usize;
            let had_hyperlink = Self::is_hyperlink_cell(&self.cell_buffer[cell_position]);
            let has_hyperlink = cell.modifier.contains(HYPERLINK_MODIFIER);

            self.cell_buffer[cell_position] = cell.clone();
            self.changed_cells.push(cell_position);

            if let Some(touched) = self.rows_touched.get_mut(y as usize) {
                *touched = true;
            }
            // Only rebuild row if hyperlink state changed on any cell
            if had_hyperlink || has_hyperlink {
                if let Some(rebuild) = self.rows_need_rebuild.get_mut(y as usize) {
                    *rebuild = true;
                }
            }

            // don't display the next cell if a fullwidth glyph preceeds it
            if cell.symbol().len() > 1 && cell.symbol().width() == 2 {
                if (cell_position + 1) < self.cells.len() {
                    let next_had_hyperlink =
                        Self::is_hyperlink_cell(&self.cell_buffer[cell_position + 1]);
                    self.cell_buffer[cell_position + 1] = Cell::new("");
                    self.changed_cells.push(cell_position + 1);
                    let next_y = (cell_position + 1) / self.size.width as usize;
                    if let Some(touched) = self.rows_touched.get_mut(next_y) {
                        *touched = true;
                    }
                    if next_had_hyperlink {
                        if let Some(rebuild) = self.rows_need_rebuild.get_mut(next_y) {
                            *rebuild = true;
                        }
                    }
                }
            }
        }

        for y in 0..row_count {
            if !self.rows_touched[y] {
                continue;
            }
            if self.rows_need_rebuild[y] {
                // Row has hyperlinks — full DOM rebuild needed
                self.render_row(y as u16)?;
            }
        }

        let width = self.size.width as usize;
        for i in self.changed_cells.iter().copied() {
            let y = i / width;
            if self.rows_need_rebuild.get(y).copied().unwrap_or(false) {
                continue;
            }
            let cell = &self.cell_buffer[i];
            Self::set_cell_content(&self.cells[i], cell, false)?;
        }

        Ok(())
    }

    /// This function is called after the [`DomBackend::draw`] function.
    ///
    /// This function does nothing because the content is directly
    /// displayed by the draw function.
    fn flush(&mut self) -> IoResult<()> {
        Ok(())
    }

    fn hide_cursor(&mut self) -> IoResult<()> {
        // Remove cursor class from the current position.
        if let Some(pos) = self.cursor_position {
            let i = (pos.y * self.size.width + pos.x) as usize;
            if i < self.cells.len() {
                let _ = self.cells[i].class_list().remove_1("rz-cursor");
            }
        }
        Ok(())
    }

    fn show_cursor(&mut self) -> IoResult<()> {
        // Cursor class is applied in set_cursor_position (called after
        // show_cursor by ratatui) because the new position isn't known yet.
        Ok(())
    }

    fn get_cursor(&mut self) -> IoResult<(u16, u16)> {
        Ok((0, 0))
    }

    fn set_cursor(&mut self, _x: u16, _y: u16) -> IoResult<()> {
        Ok(())
    }

    fn clear(&mut self) -> IoResult<()> {
        Ok(())
    }

    fn size(&self) -> IoResult<Size> {
        Ok(self.size)
    }

    fn window_size(&mut self) -> IoResult<WindowSize> {
        Ok(WindowSize {
            columns_rows: self.size,
            pixels: Size::new(
                (self.size.width as f64 * self.cell_size.0) as u16,
                (self.size.height as f64 * self.cell_size.1) as u16,
            ),
        })
    }

    fn get_cursor_position(&mut self) -> IoResult<Position> {
        match self.cursor_position {
            None => Ok((0, 0).into()),
            Some(position) => Ok(position),
        }
    }

    /// Update cursor_position, remove the CSS class from the old cell,
    /// and add it to the new cell.
    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> IoResult<()> {
        // Remove class from old position.
        if let Some(old) = self.cursor_position {
            let i = (old.y * self.size.width + old.x) as usize;
            if i < self.cells.len() {
                let _ = self.cells[i].class_list().remove_1("rz-cursor");
            }
        }

        let pos = position.into();
        self.last_cursor_position = self.cursor_position;
        self.cursor_position = Some(pos);

        // Add class to new position.
        let i = (pos.y * self.size.width + pos.x) as usize;
        if i < self.cells.len() {
            let _ = self.cells[i].class_list().add_1("rz-cursor");
        }

        Ok(())
    }

    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
        match clear_type {
            ClearType::All => self.clear(),
            _ => Err(IoError::other("unimplemented")),
        }
    }
}

impl WebEventHandler for DomBackend {
    fn on_mouse_event<F>(&mut self, callback: F) -> Result<(), Error>
    where
        F: FnMut(MouseEvent) + 'static,
    {
        use std::{cell::RefCell, rc::Rc};

        // Clear any existing handlers first
        self.clear_mouse_events();

        let config = MouseConfig::new(self.size.width, self.size.height);
        let callback = Rc::new(RefCell::new(callback));

        // Mouse event callback
        {
            let element = self.grid.clone();
            let config = config.clone();
            let callback = callback.clone();
            let mouse_callback = EventCallback::new(
                self.grid.clone(),
                MOUSE_EVENT_TYPES,
                move |event: web_sys::MouseEvent| {
                    event.prevent_default();
                    let mouse_event = create_mouse_event(&event, &element, &config);
                    (callback.borrow_mut())(mouse_event);
                },
            )?;
            self.mouse_callback = Some(mouse_callback);
        }

        // Wheel event callback
        {
            let element = self.grid.clone();
            let config = config.clone();
            let callback = callback.clone();
            let wheel_callback = EventCallback::new(
                self.grid.clone(),
                WHEEL_EVENT_TYPES,
                move |event: web_sys::WheelEvent| {
                    event.prevent_default();
                    let mouse_event = create_wheel_event(&event, &element, &config);
                    (callback.borrow_mut())(mouse_event);
                },
            )?;
            self.wheel_callback = Some(wheel_callback);
        }

        Ok(())
    }

    fn clear_mouse_events(&mut self) {
        self.mouse_callback = None;
        self.wheel_callback = None;
    }

    fn on_key_event<F>(&mut self, mut callback: F) -> Result<(), Error>
    where
        F: FnMut(KeyEvent) + 'static,
    {
        // Clear any existing handlers first
        self.clear_key_events();

        // Make the grid element focusable so it can receive key events
        self.grid.set_attribute("tabindex", "0")?;

        self.key_callback = Some(EventCallback::new(
            self.grid.clone(),
            KEY_EVENT_TYPES,
            move |event: web_sys::KeyboardEvent| {
                event.prevent_default();
                callback(event.into());
            },
        )?);

        Ok(())
    }

    fn clear_key_events(&mut self) {
        self.key_callback = None;
    }
}
