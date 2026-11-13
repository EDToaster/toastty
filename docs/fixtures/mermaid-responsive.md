# Mermaid responsive stress demo

Run `toastty md docs/fixtures/mermaid-responsive.md` inside toastty, then
resize the window between roughly 40 and 120 columns. Check that the
labels remain inside their shapes, edge labels do not cover nodes, and
the diagram does not become unreadable merely because one label is long.
The first two diagrams are positive rendering examples. The final three
are intentional stress cases: a readable source-code fallback is preferable
to tiny text when the upstream layout cannot safely wrap an edge label.
The native benchmark uses these same sources.

## Wrapped rectangular nodes

```mermaid
flowchart TD
    Request["Receive a customer document containing detailed explanations that need more than one line to stay readable in a narrow terminal"]
    Prepare["Measure each word with the actual bundled font and wrap the description before choosing the dimensions of this rectangular node"]
    Paint["Paint the visible diagram at a readable label size while preserving the colours selected by the terminal theme"]
    Request -->|"measure"| Prepare
    Prepare -->|"paint"| Paint
```

## Tall sequence for viewport painting

Only part of this diagram is visible at once. Scroll down, resize the
window, then return to a recently visited width and scroll position.

```mermaid
sequenceDiagram
    participant T as Terminal
    participant R as Renderer
    T->>R: Request 01
    R-->>T: Result 01
    T->>R: Request 02
    R-->>T: Result 02
    T->>R: Request 03
    R-->>T: Result 03
    T->>R: Request 04
    R-->>T: Result 04
    T->>R: Request 05
    R-->>T: Result 05
    T->>R: Request 06
    R-->>T: Result 06
    T->>R: Request 07
    R-->>T: Result 07
    T->>R: Request 08
    R-->>T: Result 08
    T->>R: Request 09
    R-->>T: Result 09
    T->>R: Request 10
    R-->>T: Result 10
    T->>R: Request 11
    R-->>T: Result 11
    T->>R: Request 12
    R-->>T: Result 12
    T->>R: Request 13
    R-->>T: Result 13
    T->>R: Request 14
    R-->>T: Result 14
    T->>R: Request 15
    R-->>T: Result 15
    T->>R: Request 16
    R-->>T: Result 16
    T->>R: Request 17
    R-->>T: Result 17
    T->>R: Request 18
    R-->>T: Result 18
    T->>R: Request 19
    R-->>T: Result 19
    T->>R: Request 20
    R-->>T: Result 20
    T->>R: Request 21
    R-->>T: Result 21
    T->>R: Request 22
    R-->>T: Result 22
    T->>R: Request 23
    R-->>T: Result 23
    T->>R: Request 24
    R-->>T: Result 24
    T->>R: Request 25
    R-->>T: Result 25
    T->>R: Request 26
    R-->>T: Result 26
    T->>R: Request 27
    R-->>T: Result 27
    T->>R: Request 28
    R-->>T: Result 28
    T->>R: Request 29
    R-->>T: Result 29
    T->>R: Request 30
    R-->>T: Result 30
    T->>R: Request 31
    R-->>T: Result 31
    T->>R: Request 32
    R-->>T: Result 32
    T->>R: Request 33
    R-->>T: Result 33
    T->>R: Request 34
    R-->>T: Result 34
    T->>R: Request 35
    R-->>T: Result 35
    T->>R: Request 36
    R-->>T: Result 36
    T->>R: Request 37
    R-->>T: Result 37
    T->>R: Request 38
    R-->>T: Result 38
    T->>R: Request 39
    R-->>T: Result 39
    T->>R: Request 40
    R-->>T: Result 40
```

## Long node and edge labels

```mermaid
flowchart TD
    Request["A customer asks the terminal to display a Markdown document containing a diagram with unusually long descriptive labels"]
    Check{"Can the renderer keep every word legible while fitting the available terminal width without clipping the surrounding shape?"}
    Prepare["Parse once and measure the actual bundled font instead of guessing the width from the number of characters"]
    Paint["Draw the visible portion of the prepared diagram and retain useful work while the window is being resized"]
    Request -->|"This edge label explains why measuring and wrapping text must happen before the node positions are chosen"| Check
    Check -->|"Yes: preserve readable labels and a clear connection between the decision and its destination"| Prepare
    Prepare -->|"Reuse layout and raster work when returning to a recently visited width"| Paint
```

## Wide left-to-right flow

```mermaid
flowchart LR
    A["Receive a document with detailed customer-facing descriptions"] -->|"Validate the source without discarding useful context"| B["Parse Markdown and identify every embedded Mermaid block"]
    B -->|"Keep the parsed representation available during a resize"| C["Measure labels using the bundled regular and bold fonts"]
    C -->|"Reserve space for words as well as arrowheads"| D["Place nodes and route their labelled connections"]
    D -->|"Rasterize only when the diagram is actually visible"| E["Paint the visible viewport and preserve the terminal theme"]
    E -->|"Return to an earlier width without unnecessary work"| F["Show a readable result when the terminal becomes narrow"]
```

## Sequence with descriptive participants and messages

```mermaid
sequenceDiagram
    participant User as Customer reading a detailed technical document
    participant Terminal as Terminal managing the visible Markdown block
    participant Renderer as Native Mermaid layout and raster renderer
    User->>Terminal: Resize the window while keeping the same document open and the current scroll position
    Terminal->>Renderer: Reuse the prepared diagram and calculate the size for the newly available pixel width
    Note over Terminal,Renderer: Long notes should remain readable without escaping the diagram or covering adjacent messages
    Renderer-->>Terminal: Return the fitted geometry and paint only the visible portion when it is requested
    Terminal-->>User: Display the diagram with readable labels and the colours of the active terminal theme
```
