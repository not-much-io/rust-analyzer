import * as fs from 'fs';
import * as jsonc from 'jsonc-parser';
import * as path from 'path';
import * as vscode from 'vscode';

export interface TextMateRuleSettings {
    foreground?: string;
    background?: string;
    fontStyle?: string;
}

export class ColorTheme {
    private rules: Map<string, TextMateRuleSettings> = new Map();

    static load(): ColorTheme {
        // Find out current color theme
        const themeName = vscode.workspace
            .getConfiguration('workbench')
            .get('colorTheme');

        if (typeof themeName !== 'string') {
            // console.warn('workbench.colorTheme is', themeName)
            return new ColorTheme();
        }
        return loadThemeNamed(themeName);
    }

    static fromRules(rules: TextMateRule[]): ColorTheme {
        const res = new ColorTheme();
        for (const rule of rules) {
            const scopes = typeof rule.scope === 'undefined'
                ? []
                : typeof rule.scope === 'string'
                    ? [rule.scope]
                    : rule.scope;

            for (const scope of scopes) {
                res.rules.set(scope, rule.settings);
            }
        }
        return res;
    }

    lookup(scopes: string[]): TextMateRuleSettings {
        let res: TextMateRuleSettings = {};
        for (const scope of scopes) {
            this.rules.forEach((value, key) => {
                if (scope.startsWith(key)) {
                    res = mergeRuleSettings(res, value);
                }
            });
        }
        return res;
    }

    mergeFrom(other: ColorTheme) {
        other.rules.forEach((value, key) => {
            const merged = mergeRuleSettings(this.rules.get(key), value);
            this.rules.set(key, merged);
        });
    }
}

function loadThemeNamed(themeName: string): ColorTheme {
    function isTheme(extension: vscode.Extension<unknown>): boolean {
        return (
            extension.extensionKind === vscode.ExtensionKind.UI &&
            extension.packageJSON.contributes &&
            extension.packageJSON.contributes.themes
        );
    }

    const themePaths: string[] = vscode.extensions.all
        .filter(isTheme)
        .flatMap(
            ext => ext.packageJSON.contributes.themes
                .filter((it: any) => (it.id || it.label) === themeName)
                .map((it: any) => path.join(ext.extensionPath, it.path))
        );

    const res = new ColorTheme();
    for (const themePath of themePaths) {
        res.mergeFrom(loadThemeFile(themePath));
    }

    const global_customizations: any = vscode.workspace.getConfiguration('editor').get('tokenColorCustomizations');
    res.mergeFrom(ColorTheme.fromRules(global_customizations?.textMateRules ?? []));

    const theme_customizations: any = vscode.workspace.getConfiguration('editor.tokenColorCustomizations').get(`[${themeName}]`);
    res.mergeFrom(ColorTheme.fromRules(theme_customizations?.textMateRules ?? []));


    return res;
}

function loadThemeFile(themePath: string): ColorTheme {
    let text;
    try {
        text = fs.readFileSync(themePath, 'utf8');
    } catch {
        return new ColorTheme();
    }
    const obj = jsonc.parse(text);
    const tokenColors: TextMateRule[] = obj?.tokenColors ?? [];
    const res = ColorTheme.fromRules(tokenColors);

    for (const include of obj?.include ?? []) {
        const includePath = path.join(path.dirname(themePath), include);
        res.mergeFrom(loadThemeFile(includePath));
    }

    return res;
}

interface TextMateRule {
    scope: string | string[];
    settings: TextMateRuleSettings;
}

function mergeRuleSettings(
    defaultSetting: TextMateRuleSettings | undefined,
    override: TextMateRuleSettings,
): TextMateRuleSettings {
    return {
        foreground: override.foreground ?? defaultSetting?.foreground,
        background: override.background ?? defaultSetting?.background,
        fontStyle: override.fontStyle ?? defaultSetting?.fontStyle,
    };
}
