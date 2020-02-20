import * as vscode from 'vscode';
import * as lc from 'vscode-languageclient';

import { Config } from './config';
import { createClient } from './client';

export class Ctx {
    private constructor(
        readonly config: Config,
        private readonly extCtx: vscode.ExtensionContext,
        readonly client: lc.LanguageClient
    ) {

    }

    static async create(config: Config, extCtx: vscode.ExtensionContext, serverPath: string): Promise<Ctx> {
        const client = await createClient(config, serverPath);
        const res = new Ctx(config, extCtx, client);
        res.pushCleanup(client.start());
        await client.onReady();
        return res;
    }

    get activeRustEditor(): vscode.TextEditor | undefined {
        const editor = vscode.window.activeTextEditor;
        return editor && editor.document.languageId === 'rust'
            ? editor
            : undefined;
    }

    registerCommand(name: string, factory: (ctx: Ctx) => Cmd) {
        const fullName = `rust-analyzer.${name}`;
        const cmd = factory(this);
        const d = vscode.commands.registerCommand(fullName, cmd);
        this.pushCleanup(d);
    }

    get globalState(): vscode.Memento {
        return this.extCtx.globalState;
    }

    get subscriptions(): Disposable[] {
        return this.extCtx.subscriptions;
    }

    pushCleanup(d: Disposable) {
        this.extCtx.subscriptions.push(d);
    }
}

export interface Disposable {
    dispose(): void;
}
export type Cmd = (...args: any[]) => unknown;

export async function sendRequestWithRetry<R>(
    client: lc.LanguageClient,
    method: string,
    param: unknown,
    token?: vscode.CancellationToken,
): Promise<R> {
    for (const delay of [2, 4, 6, 8, 10, null]) {
        try {
            return await (token ? client.sendRequest(method, param, token) : client.sendRequest(method, param));
        } catch (err) {
            if (delay === null || err.code !== lc.ErrorCodes.ContentModified) {
                throw err;
            }
            await sleep(10 * (1 << delay));
        }
    }
    throw 'unreachable';
}

const sleep = (ms: number) => new Promise(resolve => setTimeout(resolve, ms));
