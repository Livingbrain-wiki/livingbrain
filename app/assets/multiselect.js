// A searchable multiselect made of native checkboxes, with the choices shown
// as removable chips above the list. Native checkboxes keep it usable with a
// keyboard and a screen reader without an ARIA widget to get wrong: Tab moves
// through the search box, the chips' remove buttons and the boxes, Space
// ticks one, and the enclosing <fieldset>/<legend> names the group.
//
// Elements and text nodes only, as everywhere in `app/`: an option's label is
// a person's or an agent's name and is written as text, never as markup.
//
//   const who = createMultiSelect(fieldset, { label: "who", options, onChange });
//   who.values();             // ["member:usr_a", "agent:agt_b"]
//   who.setOptions(next);     // keeps what is still offered (and what was added by hand)
//   who.setValues([...]);  who.clear();
//
// An option is `{ value, label, group?, hint? }`. With `custom`, a person may
// add a value the list does not offer (a folder typed by hand): `custom(text)`
// answers the value to add, or throws with the reason it is not one.

/** The options whose label, hint or value contains `query`, case-blind. */
export function filterOptions(options, query) {
  const needle = String(query || "").trim().toLowerCase();
  const list = Array.isArray(options) ? options : [];
  if (!needle) return list.slice();
  return list.filter((option) =>
    [option.label, option.hint, option.value].some((text) =>
      String(text || "").toLowerCase().includes(needle),
    ),
  );
}

/** `values` with `value` added or taken out, keeping the order chosen. */
export function toggleValue(values, value, on) {
  const list = (Array.isArray(values) ? values : []).filter((v) => v !== value);
  if (on) list.push(value);
  return list;
}

function unique(values) {
  return [...new Set((Array.isArray(values) ? values : []).map(String).filter(Boolean))];
}

let nextId = 0;

/**
 * Builds the multiselect inside `root` (a <fieldset> holding its <legend>)
 * and answers its handle. `doc` is the page's document; tests pass their own.
 */
export function createMultiSelect(root, settings = {}, doc = typeof document === "undefined" ? null : document) {
  if (!root || !doc) return null;
  const {
    label = "options",
    placeholder = "Search…",
    noneText = "None chosen",
    emptyText = "Nothing matches.",
    custom = null,
    onChange = () => {},
  } = settings;
  const id = `ms${++nextId}`;
  let options = [];
  let selected = [];
  let query = "";
  const boxes = new Map();

  const make = (tag, className, text) => {
    const node = doc.createElement(tag);
    if (className) node.className = className;
    if (text != null) node.textContent = text;
    return node;
  };

  const chips = make("ul", "ms__chips");
  chips.setAttribute("aria-label", `Chosen ${label}`);
  const search = make("input", "ms__search");
  search.type = "search";
  search.id = `${id}-search`;
  search.setAttribute("autocomplete", "off");
  search.setAttribute("placeholder", placeholder);
  search.setAttribute("aria-label", `Search ${label}`);
  search.setAttribute("aria-controls", `${id}-list`);
  const tools = make("div", "ms__tools");
  const add = make("button", "btn-sm ms__add");
  add.type = "button";
  add.hidden = true;
  const all = make("button", "btn-sm ms__all", "Select shown");
  all.type = "button";
  const none = make("button", "btn-sm ms__clear", "Clear");
  none.type = "button";
  tools.appendChild(add);
  tools.appendChild(all);
  tools.appendChild(none);
  const error = make("p", "field-error ms__error");
  error.id = `${id}-error`;
  search.setAttribute("aria-describedby", error.id);
  const list = make("div", "ms__list");
  list.id = `${id}-list`;
  list.setAttribute("role", "group");
  list.setAttribute("aria-label", label);
  const empty = make("p", "ms__empty muted", emptyText);
  empty.hidden = true;
  const count = make("p", "sr");
  count.setAttribute("aria-live", "polite");

  root.classList && root.classList.add("ms");
  root.appendChild(chips);
  root.appendChild(search);
  root.appendChild(tools);
  root.appendChild(error);
  root.appendChild(list);
  root.appendChild(empty);
  root.appendChild(count);

  const labelOf = (value) => {
    const found = options.find((option) => option.value === value);
    return found ? found.label : value;
  };

  const changed = () => {
    paintChips();
    count.textContent = `${selected.length} chosen`;
    onChange(selected.slice());
  };

  function paintChips() {
    chips.replaceChildren();
    if (!selected.length) {
      chips.appendChild(make("li", "ms__none muted", noneText));
      return;
    }
    for (const value of selected) {
      const chip = make("li", "ms__chip");
      chip.appendChild(make("span", "", labelOf(value)));
      const remove = make("button", "ms__x", "×");
      remove.type = "button";
      remove.setAttribute("aria-label", `Remove ${labelOf(value)}`);
      remove.setAttribute("data-ms-remove", value);
      remove.addEventListener("click", () => {
        selected = toggleValue(selected, value, false);
        const box = boxes.get(value);
        if (box) box.checked = false;
        changed();
        search.focus && search.focus();
      });
      chip.appendChild(remove);
      chips.appendChild(chip);
    }
  }

  function customValue() {
    if (!custom || !query.trim()) return null;
    try {
      const value = custom(query);
      return value ? String(value) : null;
    } catch (reason) {
      return { error: reason && reason.message ? reason.message : String(reason) };
    }
  }

  function paintList() {
    list.replaceChildren();
    boxes.clear();
    const shown = filterOptions(options, query);
    let group = null;
    for (const option of shown) {
      if (option.group && option.group !== group) {
        group = option.group;
        list.appendChild(make("p", "ms__group", group));
      }
      const row = make("label", "ms__opt");
      const box = make("input");
      box.type = "checkbox";
      box.value = option.value;
      box.checked = selected.includes(option.value);
      box.setAttribute("data-ms-value", option.value);
      box.addEventListener("change", () => {
        selected = toggleValue(selected, option.value, box.checked);
        changed();
      });
      boxes.set(option.value, box);
      row.appendChild(box);
      row.appendChild(make("span", "ms__label", option.label));
      if (option.hint) row.appendChild(make("span", "ms__hint muted", option.hint));
      list.appendChild(row);
    }
    empty.hidden = shown.length > 0;
    const extra = customValue();
    error.textContent = extra && extra.error ? extra.error : "";
    const offer = extra && !extra.error && !options.some((option) => option.value === extra);
    add.hidden = !offer;
    if (offer) {
      add.textContent = `Add “${extra}”`;
      add.setAttribute("data-ms-add", extra);
    }
    all.hidden = shown.length === 0;
  }

  function addCustom() {
    const extra = customValue();
    if (!extra || extra.error) return false;
    if (!options.some((option) => option.value === extra)) {
      options = [...options, { value: extra, label: extra, group: "Added by hand", hint: "" }];
    }
    selected = toggleValue(selected, extra, true);
    query = "";
    search.value = "";
    paintList();
    changed();
    return true;
  }

  search.addEventListener("input", () => {
    query = search.value;
    paintList();
  });
  search.addEventListener("keydown", (event) => {
    // Enter adds what was typed (when it may be added) and never submits the
    // form around the field by accident.
    if (event.key !== "Enter") return;
    event.preventDefault();
    addCustom();
  });
  add.addEventListener("click", addCustom);
  all.addEventListener("click", () => {
    for (const option of filterOptions(options, query)) {
      selected = toggleValue(selected, option.value, true);
      const box = boxes.get(option.value);
      if (box) box.checked = true;
    }
    changed();
  });
  none.addEventListener("click", () => {
    selected = [];
    for (const box of boxes.values()) box.checked = false;
    changed();
  });

  const handle = {
    root,
    search,
    values: () => selected.slice(),
    setOptions(next) {
      const byHand = options.filter((option) => option.group === "Added by hand");
      const offered = (Array.isArray(next) ? next : []).map((option) => ({
        value: String(option.value),
        label: String(option.label == null ? option.value : option.label),
        group: option.group ? String(option.group) : "",
        hint: option.hint ? String(option.hint) : "",
      }));
      options = [...offered, ...byHand.filter((h) => !offered.some((o) => o.value === h.value))];
      selected = selected.filter((value) => options.some((option) => option.value === value));
      paintList();
      paintChips();
      count.textContent = `${selected.length} chosen`;
      return handle;
    },
    setValues(values) {
      selected = unique(values).filter((value) => options.some((option) => option.value === value));
      paintList();
      changed();
      return handle;
    },
    clear() {
      selected = [];
      query = "";
      search.value = "";
      paintList();
      changed();
      return handle;
    },
  };
  handle.setOptions(settings.options || []);
  return handle;
}
