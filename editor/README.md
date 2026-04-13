# Editor Support

## VS Code

Copy the TextMate grammar to enable syntax highlighting for `.verbose` files:

1. Open VS Code
2. Press `Ctrl+Shift+P` → "Preferences: Open User Settings (JSON)"
3. Add to your settings:

```json
"editor.tokenColorCustomizations": {
    "textMateRules": []
}
```

Or install as a local extension:

1. Copy `vscode/verbose.tmLanguage.json` to `~/.vscode/extensions/verbose/syntaxes/`
2. Create `~/.vscode/extensions/verbose/package.json`:

```json
{
    "name": "verbose",
    "version": "0.1.0",
    "engines": { "vscode": "^1.60.0" },
    "contributes": {
        "languages": [{
            "id": "verbose",
            "extensions": [".verbose"],
            "configuration": "./language-configuration.json"
        }],
        "grammars": [{
            "language": "verbose",
            "scopeName": "source.verbose",
            "path": "./syntaxes/verbose.tmLanguage.json"
        }]
    }
}
```

3. Restart VS Code

## Other Editors

The TextMate grammar (`verbose.tmLanguage.json`) is compatible with:
- Sublime Text
- Atom
- TextMate
- Any editor supporting TextMate grammars
