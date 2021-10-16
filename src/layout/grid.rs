use super::*;

/// A node that arranges its children in a grid.
#[derive(Debug)]
#[cfg_attr(feature = "layout-cache", derive(Hash))]
pub struct GridNode {
    /// The inline (columns) and block (rows) directions of this grid.
    pub dirs: Gen<Dir>,
    /// Defines sizing for content rows and columns.
    pub tracks: Gen<Vec<TrackSizing>>,
    /// Defines sizing of gutter rows and columns between content.
    pub gutter: Gen<Vec<TrackSizing>>,
    /// The nodes to be arranged in a grid.
    pub children: Vec<LayoutNode>,
}

/// Defines how to size a grid cell along an axis.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub enum TrackSizing {
    /// Fit the cell to its contents.
    Auto,
    /// A length stated in absolute values and fractions of the parent's size.
    Linear(Linear),
    /// A length that is the fraction of the remaining free space in the parent.
    Fractional(Fractional),
}

impl Layout for GridNode {
    fn layout(
        &self,
        ctx: &mut LayoutContext,
        regions: &Regions,
    ) -> Vec<Constrained<Rc<Frame>>> {
        // Prepare grid layout by unifying content and gutter tracks.
        let mut layouter = GridLayouter::new(self, regions.clone());

        // Determine all column sizes.
        layouter.measure_columns(ctx);

        // Layout the grid row-by-row.
        layouter.layout(ctx)
    }
}

impl From<GridNode> for LayoutNode {
    fn from(grid: GridNode) -> Self {
        Self::new(grid)
    }
}

/// Performs grid layout.
struct GridLayouter<'a> {
    /// The axis of the inline direction.
    inline: SpecAxis,
    /// The axis of the block direction.
    block: SpecAxis,
    /// The original expand state of the target region.
    expand: Spec<bool>,
    /// The column tracks including gutter tracks.
    cols: Vec<TrackSizing>,
    /// The row tracks including gutter tracks.
    rows: Vec<TrackSizing>,
    /// The children of the grid.
    children: &'a [LayoutNode],
    /// The regions to layout into.
    regions: Regions,
    /// Resolved column sizes.
    rcols: Vec<Length>,
    /// The full block size of the current region.
    full: Length,
    /// The used-up size of the current region. The inline size is determined
    /// once after columns are resolved and not touched again.
    used: Gen<Length>,
    /// The sum of fractional ratios in the current region.
    fr: Fractional,
    /// Rows in the current region.
    lrows: Vec<Row>,
    /// Constraints for the active region.
    constraints: Constraints,
    /// Frames for finished regions.
    finished: Vec<Constrained<Rc<Frame>>>,
}

/// Produced by initial row layout, auto and linear rows are already finished,
/// fractional rows not yet.
enum Row {
    /// Finished row frame of auto or linear row.
    Frame(Frame),
    /// Ratio of a fractional row and y index of the track.
    Fr(Fractional, usize),
}

impl<'a> GridLayouter<'a> {
    /// Prepare grid layout by unifying content and gutter tracks.
    fn new(grid: &'a GridNode, mut regions: Regions) -> Self {
        let mut cols = vec![];
        let mut rows = vec![];

        // Number of content columns: Always at least one.
        let c = grid.tracks.inline.len().max(1);

        // Number of content rows: At least as many as given, but also at least
        // as many as needed to place each item.
        let r = {
            let len = grid.children.len();
            let given = grid.tracks.block.len();
            let needed = len / c + (len % c).clamp(0, 1);
            given.max(needed)
        };

        let auto = TrackSizing::Auto;
        let zero = TrackSizing::Linear(Linear::zero());
        let get_or = |tracks: &[_], idx, default| {
            tracks.get(idx).or(tracks.last()).copied().unwrap_or(default)
        };

        // Collect content and gutter columns.
        for x in 0 .. c {
            cols.push(get_or(&grid.tracks.inline, x, auto));
            cols.push(get_or(&grid.gutter.inline, x, zero));
        }

        // Collect content and gutter rows.
        for y in 0 .. r {
            rows.push(get_or(&grid.tracks.block, y, auto));
            rows.push(get_or(&grid.gutter.block, y, zero));
        }

        // Remove superfluous gutter tracks.
        cols.pop();
        rows.pop();

        let inline = grid.dirs.inline.axis();
        let block = grid.dirs.block.axis();
        let full = regions.current.get(block);
        let rcols = vec![Length::zero(); cols.len()];

        // We use the regions only for auto row measurement and constraints.
        let expand = regions.expand;
        regions.expand = Gen::new(true, false).to_spec(block);

        Self {
            inline,
            block,
            cols,
            rows,
            children: &grid.children,
            constraints: Constraints::new(expand),
            regions,
            expand,
            rcols,
            lrows: vec![],
            full,
            used: Gen::zero(),
            fr: Fractional::zero(),
            finished: vec![],
        }
    }

    /// Determine all column sizes.
    fn measure_columns(&mut self, ctx: &mut LayoutContext) {
        enum Case {
            PurelyLinear,
            Fitting,
            Exact,
            Overflowing,
        }

        // Generic version of current and base size.
        let current = self.regions.current.get(self.inline);
        let base = self.regions.base.get(self.inline);

        // The different cases affecting constraints.
        let mut case = Case::PurelyLinear;

        // Sum of sizes of resolved linear tracks.
        let mut linear = Length::zero();

        // Sum of fractions of all fractional tracks.
        let mut fr = Fractional::zero();

        // Resolve the size of all linear columns and compute the sum of all
        // fractional tracks.
        for (&col, rcol) in self.cols.iter().zip(&mut self.rcols) {
            match col {
                TrackSizing::Auto => {
                    case = Case::Fitting;
                }
                TrackSizing::Linear(v) => {
                    let resolved = v.resolve(base);
                    *rcol = resolved;
                    linear += resolved;
                }
                TrackSizing::Fractional(v) => {
                    case = Case::Fitting;
                    fr += v;
                }
            }
        }

        // Size that is not used by fixed-size columns.
        let available = current - linear;
        if available >= Length::zero() {
            // Determine size of auto columns.
            let (auto, count) = self.measure_auto_columns(ctx, available);

            // If there is remaining space, distribute it to fractional columns,
            // otherwise shrink auto columns.
            let remaining = available - auto;
            if remaining >= Length::zero() {
                if !fr.is_zero() {
                    self.grow_fractional_columns(remaining, fr);
                    case = Case::Exact;
                }
            } else {
                self.shrink_auto_columns(available, count);
                case = Case::Exact;
            }
        } else if matches!(case, Case::Fitting) {
            case = Case::Overflowing;
        }

        // Children could depend on base.
        self.constraints.base = self.regions.base.to_spec().map(Some);

        // Set constraints depending on the case we hit.
        match case {
            Case::PurelyLinear => {}
            Case::Fitting => {
                self.constraints.min.set(self.inline, Some(self.used.inline));
            }
            Case::Exact => {
                self.constraints.exact.set(self.inline, Some(current));
            }
            Case::Overflowing => {
                self.constraints.max.set(self.inline, Some(linear));
            }
        }

        // Sum up the resolved column sizes once here.
        self.used.inline = self.rcols.iter().sum();
    }

    /// Measure the size that is available to auto columns.
    fn measure_auto_columns(
        &mut self,
        ctx: &mut LayoutContext,
        available: Length,
    ) -> (Length, usize) {
        let base = self.regions.base.get(self.block);

        let mut auto = Length::zero();
        let mut count = 0;

        // Determine size of auto columns by laying out all cells in those
        // columns, measuring them and finding the largest one.
        for (x, &col) in self.cols.iter().enumerate() {
            if col != TrackSizing::Auto {
                continue;
            }

            let mut resolved = Length::zero();
            for y in 0 .. self.rows.len() {
                if let Some(node) = self.cell(x, y) {
                    let size = Gen::new(available, Length::inf()).to_size(self.block);
                    let mut regions =
                        Regions::one(size, self.regions.base, Spec::splat(false));

                    // For fractional rows, we can already resolve the correct
                    // base, for auto it's already correct and for fr we could
                    // only guess anyway.
                    if let TrackSizing::Linear(v) = self.rows[y] {
                        regions.base.set(self.block, v.resolve(base));
                    }

                    let frame = node.layout(ctx, &regions).remove(0).item;
                    resolved.set_max(frame.size.get(self.inline));
                }
            }

            self.rcols[x] = resolved;
            auto += resolved;
            count += 1;
        }

        (auto, count)
    }

    /// Distribute remaining space to fractional columns.
    fn grow_fractional_columns(&mut self, remaining: Length, fr: Fractional) {
        for (&col, rcol) in self.cols.iter().zip(&mut self.rcols) {
            if let TrackSizing::Fractional(v) = col {
                let ratio = v / fr;
                if ratio.is_finite() {
                    *rcol = ratio * remaining;
                }
            }
        }
    }

    /// Redistribute space to auto columns so that each gets a fair share.
    fn shrink_auto_columns(&mut self, available: Length, count: usize) {
        // The fair share each auto column may have.
        let fair = available / count as f64;

        // The number of overlarge auto columns and the space that will be
        // equally redistributed to them.
        let mut overlarge: usize = 0;
        let mut redistribute = available;

        // Find out the number of and space used by overlarge auto columns.
        for (&col, rcol) in self.cols.iter().zip(&mut self.rcols) {
            if col == TrackSizing::Auto {
                if *rcol > fair {
                    overlarge += 1;
                } else {
                    redistribute -= *rcol;
                }
            }
        }

        // Redistribute the space equally.
        let share = redistribute / overlarge as f64;
        for (&col, rcol) in self.cols.iter().zip(&mut self.rcols) {
            if col == TrackSizing::Auto && *rcol > fair {
                *rcol = share;
            }
        }
    }

    /// Layout the grid row-by-row.
    fn layout(mut self, ctx: &mut LayoutContext) -> Vec<Constrained<Rc<Frame>>> {
        for y in 0 .. self.rows.len() {
            match self.rows[y] {
                TrackSizing::Auto => self.layout_auto_row(ctx, y),
                TrackSizing::Linear(v) => self.layout_linear_row(ctx, v, y),
                TrackSizing::Fractional(v) => {
                    self.fr += v;
                    self.constraints.exact.set(self.block, Some(self.full));
                    self.lrows.push(Row::Fr(v, y));
                }
            }
        }

        self.finish_region(ctx);
        self.finished
    }

    /// Layout a row with automatic size along the block axis. Such a row may
    /// break across multiple regions.
    fn layout_auto_row(&mut self, ctx: &mut LayoutContext, y: usize) {
        let base = self.regions.base.get(self.inline);
        let mut resolved: Vec<Length> = vec![];

        // Determine the size for each region of the row.
        for (x, &rcol) in self.rcols.iter().enumerate() {
            if let Some(node) = self.cell(x, y) {
                let inline = self.inline;

                let mut regions = self.regions.clone();
                regions.mutate(|size| size.set(inline, rcol));

                // Set the inline base back to the parent region's inline base
                // for auto columns.
                if self.cols[x] == TrackSizing::Auto {
                    regions.base.set(self.inline, base);
                }

                let mut sizes = node
                    .layout(ctx, &regions)
                    .into_iter()
                    .map(|frame| frame.item.size.get(self.block));

                for (target, size) in resolved.iter_mut().zip(&mut sizes) {
                    target.set_max(size);
                }

                resolved.extend(sizes);
            }
        }

        // Nothing to layout.
        if resolved.is_empty() {
            return;
        }

        // Layout into a single region.
        if let &[first] = resolved.as_slice() {
            let frame = self.layout_single_row(ctx, first, y);
            self.push_row(frame);
            return;
        }

        // Expand all but the last region if the space is not
        // eaten up by any fr rows.
        if self.fr.is_zero() {
            let len = resolved.len();
            for (target, (current, _)) in
                resolved[.. len - 1].iter_mut().zip(self.regions.iter())
            {
                target.set_max(current.get(self.block));
            }
        }

        // Layout into multiple regions.
        let frames = self.layout_multi_row(ctx, &resolved, y);
        let len = frames.len();
        for (i, frame) in frames.into_iter().enumerate() {
            self.push_row(frame);
            if i + 1 < len {
                self.constraints.exact.set(self.block, Some(self.full));
                self.finish_region(ctx);
            }
        }
    }

    /// Layout a row with linear sizing along the block axis. Such a row cannot
    /// break across multiple regions, but it may force a region break.
    fn layout_linear_row(&mut self, ctx: &mut LayoutContext, v: Linear, y: usize) {
        let base = self.regions.base.get(self.block);
        let resolved = v.resolve(base);
        let frame = self.layout_single_row(ctx, resolved, y);

        // Skip to fitting region.
        let length = frame.size.get(self.block);
        while !self.regions.current.get(self.block).fits(length)
            && !self.regions.in_full_last()
        {
            self.constraints.max.set(self.block, Some(self.used.block + length));
            self.finish_region(ctx);

            // Don't skip multiple regions for gutter and don't push a row.
            if y % 2 == 1 {
                return;
            }
        }

        self.push_row(frame);
    }

    /// Layout a row with a fixed size along the block axis and return its frame.
    fn layout_single_row(
        &self,
        ctx: &mut LayoutContext,
        block: Length,
        y: usize,
    ) -> Frame {
        let size = self.complete(block);

        let mut output = Frame::new(size, size.h);
        let mut pos = Gen::zero();

        for (x, &rcol) in self.rcols.iter().enumerate() {
            if let Some(node) = self.cell(x, y) {
                let size = Gen::new(rcol, block).to_size(self.block);
                let mut base = self.regions.base;

                // Set the base to the size for non-auto rows.
                let sizing = Gen::new(self.cols[x], self.rows[y]).to_spec(self.block);
                if sizing.x != TrackSizing::Auto {
                    base.w = size.w;
                }
                if sizing.y != TrackSizing::Auto {
                    base.h = size.h;
                }

                let regions = Regions::one(size, base, Spec::splat(true));
                let frame = node.layout(ctx, &regions).remove(0);
                output.push_frame(pos.to_point(self.block), frame.item);
            }

            pos.inline += rcol;
        }

        output
    }

    /// Layout a row spanning multiple regions.
    fn layout_multi_row(
        &self,
        ctx: &mut LayoutContext,
        resolved: &[Length],
        y: usize,
    ) -> Vec<Frame> {
        let base = self.regions.base.get(self.inline);

        // Prepare frames.
        let mut outputs: Vec<_> = resolved
            .iter()
            .map(|&v| self.complete(v))
            .map(|size| Frame::new(size, size.h))
            .collect();

        // Prepare regions.
        let size = self.complete(resolved[0]);
        let mut regions = Regions::one(size, self.regions.base, Spec::splat(true));
        regions.backlog = resolved[1 ..]
            .iter()
            .map(|&v| self.complete(v))
            .collect::<Vec<_>>()
            .into_iter();

        // Layout the row.
        let mut pos = Gen::zero();
        for (x, &rcol) in self.rcols.iter().enumerate() {
            if let Some(node) = self.cell(x, y) {
                regions.mutate(|size| size.set(self.inline, rcol));

                // Set the inline base back to the parent region's inline base
                // for auto columns.
                if self.cols[x] == TrackSizing::Auto {
                    regions.base.set(self.inline, base);
                }

                // Push the layouted frames into the individual output frames.
                let frames = node.layout(ctx, &regions);
                for (output, frame) in outputs.iter_mut().zip(frames) {
                    output.push_frame(pos.to_point(self.block), frame.item);
                }
            }

            pos.inline += rcol;
        }

        outputs
    }

    /// Push a row frame into the current region.
    fn push_row(&mut self, frame: Frame) {
        let length = frame.size.get(self.block);
        *self.regions.current.get_mut(self.block) -= length;
        self.used.block += length;
        self.lrows.push(Row::Frame(frame));
    }

    /// Finish rows for one region.
    fn finish_region(&mut self, ctx: &mut LayoutContext) {
        // Determine the block size of the region.
        let block = if self.fr.is_zero() || self.full.is_infinite() {
            self.used.block
        } else {
            self.full
        };

        let size = self.complete(block);
        self.constraints.min.set(self.block, Some(block));

        // The frame for the region.
        let mut output = Frame::new(size, size.h);
        let mut pos = Gen::zero();

        // Determine the remaining size for fractional rows.
        let remaining = self.full - self.used.block;

        // Place finished rows and layout fractional rows.
        for row in std::mem::take(&mut self.lrows) {
            let frame = match row {
                Row::Frame(frame) => frame,
                Row::Fr(v, y) => {
                    let ratio = v / self.fr;
                    if remaining.is_finite() && ratio.is_finite() {
                        let resolved = ratio * remaining;
                        self.layout_single_row(ctx, resolved, y)
                    } else {
                        continue;
                    }
                }
            };

            let point = pos.to_point(self.block);
            pos.block += frame.size.get(self.block);
            output.merge_frame(point, frame);
        }

        self.regions.next();
        self.full = self.regions.current.get(self.block);
        self.used.block = Length::zero();
        self.fr = Fractional::zero();
        self.finished.push(output.constrain(self.constraints));
        self.constraints = Constraints::new(self.expand);
    }

    /// Get the node in the cell in column `x` and row `y`.
    ///
    /// Returns `None` if it's a gutter cell.
    #[track_caller]
    fn cell(&self, x: usize, y: usize) -> Option<&'a LayoutNode> {
        assert!(x < self.cols.len());
        assert!(y < self.rows.len());

        // Even columns and rows are children, odd ones are gutter.
        if x % 2 == 0 && y % 2 == 0 {
            let c = 1 + self.cols.len() / 2;
            self.children.get((y / 2) * c + x / 2)
        } else {
            None
        }
    }

    /// Return a size where the inline axis spans the whole grid and the block
    /// axis the given length.
    fn complete(&self, block: Length) -> Size {
        Gen::new(self.used.inline, block).to_size(self.block)
    }
}
