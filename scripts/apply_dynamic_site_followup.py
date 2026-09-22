#!/usr/bin/env python3
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
PATH = ROOT / "crates/comptrol-core/src/browser.rs"
text = PATH.read_text()

old = '''    let url_satisfied = match (url, url_contains) {
        (Some(expected), _) => current_url == expected,
        (None, Some(fragment)) => current_url.contains(fragment),
        (None, None) => true,
    };
'''
new = '''    let url_satisfied = match (url, url_contains) {
        (Some(expected), Some(fragment)) => {
            current_url == expected || current_url.contains(fragment)
        }
        (Some(expected), None) => current_url == expected,
        (None, Some(fragment)) => current_url.contains(fragment),
        (None, None) => true,
    };
'''
if text.count(old) != 1:
    raise SystemExit(f"ensure_state URL match block count={text.count(old)}")
text = text.replace(old, new, 1)

old_name = '''            const nameOf = element => normalize(
                element.getAttribute('aria-label') ||
                element.getAttribute('title') ||
                element.innerText ||
                element.textContent
            );
'''
new_name = r'''            const labelledByText = element => normalize(
                (element.getAttribute('aria-labelledby') || '')
                    .split(/\s+/).filter(Boolean)
                    .map(id => element.ownerDocument.getElementById(id))
                    .filter(Boolean)
                    .map(node => node.innerText || node.textContent || '')
                    .join(' ')
            );
            const labelsText = element => normalize(
                element.labels
                    ? [...element.labels].map(label => label.innerText || label.textContent || '').join(' ')
                    : ''
            );
            const nameOf = element => normalize(
                element.getAttribute('aria-label') ||
                labelledByText(element) ||
                labelsText(element) ||
                element.getAttribute('placeholder') ||
                element.getAttribute('title') ||
                element.innerText ||
                element.textContent
            );
'''
if text.count(old_name) != 2:
    raise SystemExit(f"semantic name block count={text.count(old_name)}")
text = text.replace(old_name, new_name)

old_compact = '''            const nameOf = element => normalize(
                element.getAttribute('aria-label') ||
                element.getAttribute('title') ||
                element.getAttribute('placeholder') ||
                element.innerText ||
                element.textContent
            );
'''
new_compact = r'''            const labelledByText = element => normalize(
                (element.getAttribute('aria-labelledby') || '')
                    .split(/\s+/).filter(Boolean)
                    .map(id => element.ownerDocument.getElementById(id))
                    .filter(Boolean)
                    .map(node => node.innerText || node.textContent || '')
                    .join(' ')
            );
            const labelsText = element => normalize(
                element.labels
                    ? [...element.labels].map(label => label.innerText || label.textContent || '').join(' ')
                    : ''
            );
            const nameOf = element => normalize(
                element.getAttribute('aria-label') ||
                labelledByText(element) ||
                labelsText(element) ||
                element.getAttribute('placeholder') ||
                element.getAttribute('title') ||
                element.innerText ||
                element.textContent
            );
'''
if text.count(old_compact) != 1:
    raise SystemExit(f"compact name block count={text.count(old_compact)}")
text = text.replace(old_compact, new_compact, 1)

old_verify = '''                        setNativeValue(element, desired);
                        await Promise.resolve();
                        const verified = readValue(element) === desired;
'''
new_verify = '''                        setNativeValue(element, desired);
                        await Promise.resolve();
                        await new Promise(requestAnimationFrame);
                        const verified = readValue(element) === desired;
'''
if text.count(old_verify) != 1:
    raise SystemExit(f"fill verify block count={text.count(old_verify)}")
text = text.replace(old_verify, new_verify, 1)

PATH.write_text(text)
print("dynamic-site follow-up applied")
