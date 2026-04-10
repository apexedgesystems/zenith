# Zenith UI Design Specification

## Research Sources

Patterns synthesized from: OpenC3/COSMOS, NASA Open MCT, Grafana,
Chronograf/InfluxDB, YAMCS, SatNOGS.

## Global Shell

- **Sidebar**: 56px wide, icon-only (expand on hover). Active page: left cyan accent bar
- **Top Bar**: 48px. Left: target selector dropdown. Center: connection status pill. Right: UTC clock
- **Target Selector**: Persists across page navigation (YAMCS instance pattern)

## Color System (Dark Theme)

| Token          | Hex     | Usage                 |
| -------------- | ------- | --------------------- |
| bg-body        | #0d1117 | Page background       |
| bg-surface     | #161b22 | Cards, panels         |
| bg-elevated    | #1c2128 | Dropdowns, hover      |
| bg-input       | #0d1117 | Input fields          |
| border-default | #30363d | Panel borders         |
| border-muted   | #21262d | Subtle separators     |
| text-primary   | #e6edf3 | Primary text          |
| text-secondary | #8b949e | Labels                |
| text-muted     | #6e7681 | Disabled, timestamps  |
| accent-cyan    | #58a6ff | Active nav, links     |
| status-green   | #3fb950 | Healthy, connected    |
| status-yellow  | #d29922 | Warning, caution      |
| status-red     | #f85149 | Error, critical       |
| chart-1        | #58a6ff | First series (blue)   |
| chart-2        | #3fb950 | Second series (green) |
| chart-3        | #d29922 | Third series (amber)  |
| chart-4        | #f778ba | Fourth series (pink)  |
| chart-5        | #bc8cff | Fifth series (purple) |

## Typography

- **Labels/nav**: Inter (sans-serif)
- **Numeric values**: JetBrains Mono or IBM Plex Mono (monospace)
- Operators need to distinguish 0/O and 1/l at a glance

## Page Designs

### Dashboard

- Status row: 8 stat cards (Grafana pattern) with component health
- Color-coded dots (green/yellow/red) per component
- Sparkline trends in each card
- Command log + telemetry subscription table

### Telemetry

- Time controls bar (Open MCT Time Conductor pattern)
- Channel picker sidebar (tree view by component)
- Stacked resizable uPlot charts
- Drag-and-drop channel to plot (Open MCT composition)

### Commanding

- Two cascading dropdowns: Target component -> Command opcode (COSMOS pattern)
- Auto-generated parameter forms from struct dicts
- Hazardous command confirmation (COSMOS pattern)
- Command history with re-execute on click

### Configuration

- System info card
- Component registry table with status dots
- Connection event log

### Parameters

- Component selector with bank info (A/B)
- Editable parameter table with validation
- Modified values highlighted
- Upload + Apply with progress

### Files

- File transfer panel with progress bar
- Local data browser (sessions, CSV/JSON export)
- Transfer history

## Implementation

- React + Tailwind CSS (custom dark theme)
- uPlot for charts (uplot-react wrapper, memoized options)
- Inter + JetBrains Mono fonts
- Ring buffer for telemetry (Float64Array)
- React Context for global state (connection, vehicle, clock)
