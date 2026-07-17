import QtQuick
import QtQuick.Layouts
import Quickshell
import Quickshell.Widgets
import qs.Commons
import qs.Services.Compositor
import qs.Services.UI

Item {
    id: root

    property var pluginApi: null
    property ShellScreen screen
    property string widgetId: ""
    property string section: ""
    property int sectionWidgetIndex: -1
    property int sectionWidgetsCount: 0
    property string targetOutput: ""

    readonly property string screenName: screen?.name ?? ""
    readonly property var widgetSettings: {
        if (section && sectionWidgetIndex >= 0 && screenName) {
            const sections = Settings.getBarWidgetsForScreen(screenName);
            const widgets = sections ? sections[section] : null;
            if (widgets && sectionWidgetIndex < widgets.length) return widgets[sectionWidgetIndex];
        }
        return {};
    }
    readonly property string outputName: widgetSettings.targetOutput || targetOutput || screenName
    readonly property string barPosition: Settings.getBarPositionForScreen(screenName)
    readonly property bool vertical: barPosition === "left" || barPosition === "right"
    readonly property real capsuleHeight: Style.getCapsuleHeightForScreen(screenName)
    readonly property real iconSize: Math.max(16, Math.round(capsuleHeight * (pluginApi?.pluginSettings?.iconScale ?? 0.8)))
    readonly property real groupPadding: Math.max(3, Math.round(Style.marginXS * 0.7))
    readonly property real groupGap: Style.marginXS
    readonly property real iconGap: 3
    readonly property bool hideEmpty: pluginApi?.pluginSettings?.hideEmptyWorkspaces ?? false
    readonly property bool allowDrag: pluginApi?.pluginSettings?.allowDrag ?? true
    readonly property bool showActiveIndicator: pluginApi?.pluginSettings?.showActiveIndicator ?? true
    readonly property real groupOpacity: pluginApi?.pluginSettings?.groupOpacity ?? 0.4
    readonly property string backendCommand: pluginApi?.pluginSettings?.backendCommand ?? "niri-xmb"

    property var localWorkspaces: []

    implicitWidth: vertical ? capsuleHeight : workspaceFlow.implicitWidth
    implicitHeight: vertical ? workspaceFlow.implicitHeight : capsuleHeight

    function windowsFor(workspaceId) {
        return CompositorService.getWindowsForWorkspace(workspaceId);
    }

    function refresh() {
        if (!outputName) return;
        const wanted = [];
        const target = outputName.toLowerCase();
        for (let i = 0; i < CompositorService.workspaces.count; ++i) {
            const ws = CompositorService.workspaces.get(i);
            if (!ws.output || ws.output.toLowerCase() !== target) continue;
            const windows = windowsFor(ws.id);
            if (hideEmpty && windows.length === 0 && !ws.isActive && !ws.isFocused) continue;
            wanted.push({
                "id": ws.id,
                "idx": ws.idx,
                "name": ws.name ?? "",
                "output": ws.output,
                "isActive": ws.isActive,
                "isFocused": ws.isFocused,
                "isUrgent": ws.isUrgent,
                "windows": windows
            });
        }
        localWorkspaces = wanted;
    }

    function focusWorkspace(workspace) {
        CompositorService.switchToWorkspace(workspace);
    }

    function focusWindow(windowData) {
        CompositorService.focusWindow(windowData);
    }

    function dropWindow(sourceId, workspaceId, targetId, fraction) {
        if (!allowDrag || !sourceId || !workspaceId) return;
        const args = [
            backendCommand,
            "drop",
            "--source-window-id", String(sourceId),
            "--target-workspace-id", String(workspaceId),
            "--fraction", String(Math.max(0, Math.min(1, fraction)))
        ];
        if (targetId) args.push("--target-window-id", String(targetId));
        Quickshell.execDetached(args);
    }

    function switchByOffset(offset) {
        if (localWorkspaces.length < 2) return;
        let current = 0;
        for (let i = 0; i < localWorkspaces.length; ++i) {
            const ws = localWorkspaces[i];
            if (ws.isFocused || ws.isActive) {
                current = i;
                break;
            }
        }
        let next = (current + offset) % localWorkspaces.length;
        if (next < 0) next += localWorkspaces.length;
        focusWorkspace(localWorkspaces[next]);
    }

    Component.onCompleted: Qt.callLater(refresh)
    onScreenNameChanged: Qt.callLater(refresh)
    onTargetOutputChanged: Qt.callLater(refresh)
    onHideEmptyChanged: Qt.callLater(refresh)

    Connections {
        target: CompositorService
        function onWorkspaceChanged() { Qt.callLater(root.refresh); }
        function onWindowListChanged() { Qt.callLater(root.refresh); }
        function onActiveWindowChanged() { Qt.callLater(root.refresh); }
    }

    WheelHandler {
        target: root
        acceptedDevices: PointerDevice.Mouse | PointerDevice.TouchPad
        onWheel: event => {
            const delta = Math.abs(event.angleDelta.y) >= Math.abs(event.angleDelta.x)
                ? event.angleDelta.y : event.angleDelta.x;
            if (delta === 0) return;
            root.switchByOffset(delta > 0 ? -1 : 1);
            event.accepted = true;
        }
    }

    Flow {
        id: workspaceFlow
        anchors.centerIn: parent
        flow: root.vertical ? Flow.TopToBottom : Flow.LeftToRight
        spacing: root.groupGap

        Repeater {
            model: root.localWorkspaces

            delegate: Rectangle {
                id: workspaceGroup
                required property var modelData
                readonly property var workspace: modelData
                readonly property var windowList: workspace.windows ?? []
                property bool dropHovered: false

                width: root.vertical
                    ? root.capsuleHeight
                    : Math.max(root.iconSize + root.groupPadding * 2,
                               windowList.length * root.iconSize
                               + Math.max(0, windowList.length - 1) * root.iconGap
                               + root.groupPadding * 2)
                height: root.vertical
                    ? Math.max(root.iconSize + root.groupPadding * 2,
                               windowList.length * root.iconSize
                               + Math.max(0, windowList.length - 1) * root.iconGap
                               + root.groupPadding * 2)
                    : root.capsuleHeight
                radius: Style.radiusS
                color: Qt.alpha(dropHovered || workspace.isFocused
                                ? Color.mPrimary : Color.mSurfaceVariant,
                                dropHovered || workspace.isFocused ? 0.15 : root.groupOpacity)
                border.width: Style.borderS
                border.color: Qt.alpha(workspace.isUrgent ? Color.mError
                                            : (dropHovered || workspace.isFocused ? Color.mPrimary : Color.mOutline),
                                           dropHovered || workspace.isFocused ? 0.7 : 0.3)

                Behavior on width { NumberAnimation { duration: Style.animationFast; easing.type: Easing.OutCubic } }
                Behavior on height { NumberAnimation { duration: Style.animationFast; easing.type: Easing.OutCubic } }

                TapHandler {
                    acceptedButtons: Qt.LeftButton
                    onTapped: root.focusWorkspace(workspaceGroup.workspace)
                }

                DropArea {
                    anchors.fill: parent
                    keys: ["niri-xmb-window"]
                    onEntered: workspaceGroup.dropHovered = true
                    onExited: workspaceGroup.dropHovered = false
                    onDropped: drop => {
                        workspaceGroup.dropHovered = false;
                        root.dropWindow(drop.source?.windowData?.id,
                                        workspaceGroup.workspace.id,
                                        0,
                                        root.vertical
                                            ? drop.y / Math.max(1, height)
                                            : drop.x / Math.max(1, width));
                    }
                }

                Flow {
                    anchors.centerIn: parent
                    flow: root.vertical ? Flow.TopToBottom : Flow.LeftToRight
                    spacing: root.iconGap

                    Repeater {
                        model: workspaceGroup.windowList

                        delegate: Item {
                            id: taskItem
                            required property var modelData
                            property var windowData: modelData
                            property bool dropHovered: false
                            width: root.iconSize
                            height: root.iconSize
                            z: dragMouse.drag.active ? 20 : 1

                            IconImage {
                                anchors.fill: parent
                                source: ThemeIcons.iconForAppId(taskItem.windowData?.appId?.toLowerCase())
                                smooth: true
                                asynchronous: true
                                opacity: dragMouse.drag.active ? 0.35 : 1.0
                            }

                            Rectangle {
                                visible: root.showActiveIndicator && (taskItem.windowData?.isFocused || taskItem.dropHovered)
                                anchors.horizontalCenter: parent.horizontalCenter
                                anchors.bottom: parent.bottom
                                anchors.bottomMargin: -1
                                width: Math.max(4, root.iconSize * 0.25)
                                height: 3
                                radius: height / 2
                                color: taskItem.windowData?.isFocused ? Color.mPrimary : Color.mHover
                            }

                            Item {
                                id: dragProxy
                                x: 0
                                y: 0
                                width: parent.width
                                height: parent.height
                                visible: dragMouse.drag.active
                                z: 100
                                Drag.active: dragMouse.drag.active
                                Drag.source: taskItem
                                Drag.keys: ["niri-xmb-window"]
                                Drag.hotSpot.x: width / 2
                                Drag.hotSpot.y: height / 2

                                IconImage {
                                    anchors.fill: parent
                                    source: ThemeIcons.iconForAppId(taskItem.windowData?.appId?.toLowerCase())
                                    smooth: true
                                }
                            }

                            MouseArea {
                                id: dragMouse
                                anchors.fill: parent
                                acceptedButtons: Qt.LeftButton

                                drag.target: root.allowDrag ? dragProxy : null
                                drag.axis: Drag.XAndYAxis
                                preventStealing: true

                                onReleased: {
                                    if (dragProxy.Drag.active)
                                        dragProxy.Drag.drop();
                                    Qt.callLater(() => {
                                        dragProxy.x = 0;
                                        dragProxy.y = 0;
                                    });
                                }

                                onClicked: root.focusWindow(taskItem.windowData)
                            }

                            DropArea {
                                anchors.fill: parent
                                keys: ["niri-xmb-window"]
                                onEntered: taskItem.dropHovered = true
                                onExited: taskItem.dropHovered = false
                                onDropped: drop => {
                                    taskItem.dropHovered = false;
                                    root.dropWindow(drop.source?.windowData?.id,
                                                    workspaceGroup.workspace.id,
                                                    taskItem.windowData?.id,
                                                    root.vertical
                                                        ? drop.y / Math.max(1, height)
                                                        : drop.x / Math.max(1, width));
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
