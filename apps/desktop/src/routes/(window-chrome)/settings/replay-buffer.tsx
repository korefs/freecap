import { invoke } from "@tauri-apps/api/core";
import { Store } from "@tauri-apps/plugin-store";
import { cx } from "cva";
import {
	createMemo,
	createResource,
	createSignal,
	For,
	onCleanup,
	onMount,
	Show,
} from "solid-js";
import { Toggle } from "~/components/Toggle";
import { Section, SectionCard, SettingsPageContent } from "./Setting";

type ReplayWindowRule = {
	ownerName: string;
	windowTitle: string;
	executablePath?: string | null;
};

type ReplayBufferSettings = {
	enabled: boolean;
	clipDurationSecs: 30 | 60 | 180 | 300;
	targets: ReplayWindowRule[];
};

type CaptureWindow = {
	id: unknown;
	owner_name?: string;
	ownerName?: string;
	name: string;
	executable_path?: string | null;
	executablePath?: string | null;
};

type Hotkey = {
	code: string;
	meta: boolean;
	ctrl: boolean;
	alt: boolean;
	shift: boolean;
};

type HotkeysStore = {
	hotkeys?: Record<string, Hotkey>;
};

const DEFAULT_SETTINGS: ReplayBufferSettings = {
	enabled: false,
	clipDurationSecs: 30,
	targets: [],
};

const DURATIONS = [
	{ value: 30, label: "30s" },
	{ value: 60, label: "1min" },
	{ value: 180, label: "3min" },
	{ value: 300, label: "5min" },
] satisfies { value: ReplayBufferSettings["clipDurationSecs"]; label: string }[];

const MODIFIER_KEYS = new Set(["Meta", "Shift", "Control", "Alt"]);

let storePromise: Promise<Store> | undefined;
const getStore = () => {
	if (!storePromise) storePromise = Store.load("store");
	return storePromise;
};

async function readReplaySettings() {
	const store = await getStore();
	return {
		...DEFAULT_SETTINGS,
		...((await store.get<Partial<ReplayBufferSettings>>(
			"replay_buffer_settings",
		)) ?? {}),
	};
}

async function writeReplaySettings(settings: ReplayBufferSettings) {
	const store = await getStore();
	await store.set("replay_buffer_settings", settings);
	await store.save();
}

async function readHotkey() {
	const store = await getStore();
	const hotkeys = await store.get<HotkeysStore>("hotkeys");
	return hotkeys?.hotkeys?.clipReplayBuffer ?? null;
}

async function writeHotkey(hotkey: Hotkey | null) {
	const store = await getStore();
	const current = (await store.get<HotkeysStore>("hotkeys")) ?? { hotkeys: {} };
	const next = { ...(current.hotkeys ?? {}) };
	if (hotkey) next.clipReplayBuffer = hotkey;
	else delete next.clipReplayBuffer;
	await store.set("hotkeys", { hotkeys: next });
	await store.save();
	await invoke("set_hotkey", {
		action: "clipReplayBuffer",
		hotkey,
	});
}

function ownerName(window: CaptureWindow) {
	return window.ownerName ?? window.owner_name ?? "";
}

function executablePath(window: CaptureWindow) {
	return window.executablePath ?? window.executable_path ?? null;
}

function ruleFromWindow(window: CaptureWindow): ReplayWindowRule {
	return {
		ownerName: ownerName(window),
		windowTitle: window.name,
		executablePath: executablePath(window),
	};
}

function sameRule(a: ReplayWindowRule, b: ReplayWindowRule) {
	if (a.executablePath && b.executablePath) {
		return a.executablePath.toLowerCase() === b.executablePath.toLowerCase();
	}

	return (
		a.ownerName.toLowerCase() === b.ownerName.toLowerCase() &&
		a.windowTitle.toLowerCase() === b.windowTitle.toLowerCase()
	);
}

export default function ReplayBufferSettingsPage() {
	const [settings, { refetch }] = createResource(readReplaySettings);
	const [windows, { refetch: refetchWindows }] = createResource(() =>
		invoke<CaptureWindow[]>("list_capture_windows").catch(() => []),
	);
	const [hotkey, setHotkey] = createSignal<Hotkey | null>(null);
	const [listening, setListening] = createSignal(false);

	onMount(() => {
		void readHotkey().then(setHotkey);
		const handleKeyDown = async (event: KeyboardEvent) => {
			if (!listening() || MODIFIER_KEYS.has(event.key)) return;
			event.preventDefault();
			const next = {
				code: event.code,
				meta: event.metaKey,
				ctrl: event.ctrlKey,
				alt: event.altKey,
				shift: event.shiftKey,
			};
			setHotkey(next);
			setListening(false);
			await writeHotkey(next);
		};
		window.addEventListener("keydown", handleKeyDown);
		onCleanup(() => window.removeEventListener("keydown", handleKeyDown));
	});

	const currentSettings = createMemo(() => settings() ?? DEFAULT_SETTINGS);

	const updateSettings = async (patch: Partial<ReplayBufferSettings>) => {
		const next = { ...currentSettings(), ...patch };
		await writeReplaySettings(next);
		await refetch();
	};

	const selectedWindows = createMemo(() => currentSettings().targets);

	return (
		<div class="cap-settings-page flex flex-col h-full custom-scroll">
			<SettingsPageContent>
				<Section
					title="Replay Buffer"
					description="Automatically keeps a rolling local buffer for selected game windows."
				>
					<SectionCard class="divide-y divide-gray-3">
						<div class="flex justify-between items-center px-4 py-3.5 gap-4">
							<div class="flex flex-col gap-0.5 min-w-0">
								<p class="text-[13px] text-gray-12">Enable replay buffer</p>
								<p class="text-xs leading-snug text-gray-10">
									Starts when one of the selected windows is available.
								</p>
							</div>
							<Toggle
								size="sm"
								checked={currentSettings().enabled}
								onChange={(enabled) => updateSettings({ enabled })}
							/>
						</div>
						<div class="flex justify-between items-center px-4 py-3.5 gap-4">
							<div class="flex flex-col gap-0.5 min-w-0">
								<p class="text-[13px] text-gray-12">Clip length</p>
								<p class="text-xs leading-snug text-gray-10">
									Saved clips are rounded outward to segment boundaries.
								</p>
							</div>
							<div class="inline-flex p-0.5 rounded-lg border border-gray-3 bg-gray-3">
								<For each={DURATIONS}>
									{(duration) => (
										<button
											type="button"
											onClick={() =>
												updateSettings({
													clipDurationSecs: duration.value,
												})
											}
											class={cx(
												"px-3 py-1 text-xs font-medium rounded-md transition-[background-color,color,box-shadow]",
												currentSettings().clipDurationSecs === duration.value
													? "bg-gray-1 text-gray-12 shadow-sm"
													: "text-gray-10 hover:text-gray-12",
											)}
										>
											{duration.label}
										</button>
									)}
								</For>
							</div>
						</div>
						<div class="flex justify-between items-center px-4 py-3.5 gap-4">
							<div class="flex flex-col gap-0.5 min-w-0">
								<p class="text-[13px] text-gray-12">Clip shortcut</p>
								<p class="text-xs leading-snug text-gray-10">
									Defaults to PageUp when that shortcut is available.
								</p>
							</div>
							<div class="flex items-center gap-2">
								<button
									type="button"
									class="text-sm bg-transparent rounded-lg"
									onClick={() => setListening(true)}
								>
									<HotkeyText binding={hotkey()} listening={listening()} />
								</button>
								<button
									type="button"
									class="text-xs text-gray-10 hover:text-gray-12"
									onClick={async () => {
										setHotkey(null);
										setListening(false);
										await writeHotkey(null);
									}}
								>
									Clear
								</button>
							</div>
						</div>
					</SectionCard>
				</Section>

				<Section
					title="Game windows"
					description="Select the windows that should start replay capture automatically."
					right={
						<button
							type="button"
							class="text-xs text-gray-10 hover:text-gray-12"
							onClick={() => refetchWindows()}
						>
							Refresh
						</button>
					}
				>
					<SectionCard class="divide-y divide-gray-3">
						<Show
							when={(windows() ?? []).length > 0}
							fallback={
								<p class="px-4 py-4 text-xs text-gray-10">
									No capturable windows found.
								</p>
							}
						>
							<For each={windows() ?? []}>
								{(window) => {
									const rule = () => ruleFromWindow(window);
									const selected = () =>
										selectedWindows().some((item) => sameRule(item, rule()));

									return (
										<button
											type="button"
											class="flex justify-between items-center w-full px-4 py-3.5 text-left gap-4 hover:bg-gray-3 transition-colors"
											onClick={() => {
												const current = selectedWindows();
												const nextTargets = selected()
													? current.filter((item) => !sameRule(item, rule()))
													: [...current, rule()];
												updateSettings({ targets: nextTargets });
											}}
										>
											<div class="flex flex-col gap-0.5 min-w-0">
												<p class="text-[13px] text-gray-12 truncate">
													{window.name}
												</p>
												<p class="text-xs text-gray-10 truncate">
													{ownerName(window)}
												</p>
											</div>
											<div
												class={cx(
													"size-4 rounded-full border flex items-center justify-center shrink-0",
													selected()
														? "bg-blue-9 border-blue-9"
														: "border-gray-7",
												)}
											>
												<Show when={selected()}>
													<div class="size-1.5 rounded-full bg-white" />
												</Show>
											</div>
										</button>
									);
								}}
							</For>
						</Show>
					</SectionCard>
				</Section>
			</SettingsPageContent>
		</div>
	);
}

function HotkeyText(props: { binding: Hotkey | null; listening: boolean }) {
	if (props.listening) {
		return (
			<p class="flex items-center text-[11px] uppercase transition-colors cursor-pointer py-3 px-2.5 h-5 bg-gray-4 border border-gray-5 rounded-lg text-gray-11">
				Set hotkey...
			</p>
		);
	}

	if (!props.binding) {
		return (
			<p class="flex items-center text-[11px] uppercase transition-colors hover:bg-gray-6 hover:border-gray-7 cursor-pointer py-3 px-2.5 h-5 bg-gray-4 border border-gray-5 rounded-lg text-gray-11 hover:text-gray-12">
				None
			</p>
		);
	}

	const keys: string[] = [];
	if (props.binding.meta) keys.push("Win");
	if (props.binding.ctrl) keys.push("Ctrl");
	if (props.binding.alt) keys.push("Alt");
	if (props.binding.shift) keys.push("Shift");
	keys.push(
		props.binding.code.startsWith("Key")
			? props.binding.code[3]
			: props.binding.code,
	);

	return (
		<div class="flex gap-1 items-center w-fit group">
			<For each={keys}>
				{(key) => (
					<kbd class="inline-flex justify-center w-fit text-xs items-center p-2 text-[13px] font-medium rounded-sm border size-6 text-gray-11 bg-gray-5 border-gray-6 group-hover:border-gray-8 transition-colors duration-200 group-hover:bg-gray-7">
						{key}
					</kbd>
				)}
			</For>
		</div>
	);
}
