# Toastty Markdown

A **fast**, *GPU-accelerated* terminal that renders ~~plain~~ rich Markdown
inline. Use `toastty md README.md` to try it, or read the [docs](https://example.com/docs).

## Features

- Headings, **bold**, *italic*, `inline code` and [links](https://example.com)
- Nested lists:
  - second level with a long item that needs to wrap onto the next line nicely
    - third level
- Ordered lists:
  1. first
  2. second

### Tasks

- [x] parse with pulldown-cmark
- [ ] draw maths
- [ ] draw diagrams

> Block quotes are drawn with a bar on the left and muted text.
>
> > Nested quotes work too.

```rust
// Highlighted with syntect.
fn main() {
    let greeting = "hello";
    println!("{greeting}, world: {}", 42);
}
```

| Element | Status | Notes |
| :------ | :----: | ----: |
| Tables  | done   | aligned columns |
| Images  | partial | data: URIs only |

---

#### Smaller heading

Text after a rule, with an image placeholder: ![logo](https://example.com/logo.png)
and an embedded image: ![gradient](data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAGAAAAAgCAYAAADtwH1UAAAAeElEQVR42u3WuQ2AMBBFweUohg7cGi26B3oxHWAJgh8wTyIyG4288nKc11hr1MPXJ+fN/Pv5vbZSMAAAAAgAAAH4L0Cf/NMm5+Y/zLsBVhAAAQAgAAAUAvBOD867AVYQAAEAIAAAFALwTg/OuwFWEAABACAAABToBhhwen7iGRoOAAAAAElFTkSuQmCC)
