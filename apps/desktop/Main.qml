import QtQuick

Window {
    id: window

    required property bool callActive

    width: 480
    height: 320
    visible: true
    title: qsTr("Skyber")
    color: "#101824"

    Text {
        anchors.centerIn: parent
        color: "#f3f7fb"
        text: window.callActive ? qsTr("Call active") : qsTr("No active call")
    }
}
