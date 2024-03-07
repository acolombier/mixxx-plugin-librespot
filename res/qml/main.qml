import QtQuick 2
import QtQuick.Layouts 1

Item {
    id: root

    Rectangle {
        anchors.fill: parent

        gradient: Gradient {
            GradientStop { position: 0.0; color: "#222" }
            GradientStop { position: 0.8; color: "#222" }
            GradientStop { position: 1.0; color: "#25d666" }
        }

        ColumnLayout {
            anchors.fill: parent
            anchors.margins: 10

            Item {
                Layout.fillWidth: true
                Layout.fillHeight: true
            }

            Rectangle {
                Layout.alignment: Qt.AlignCenter

                color: "white"
                Layout.preferredWidth: 340
                Layout.preferredHeight: 40
                radius: 20

                TextInput {
                    id: username
                    anchors.fill: parent
                    anchors.margins: 10
                    anchors.leftMargin: 20
                    anchors.rightMargin: 20

                    width: parent.implicitWidth

                    property string placeholderText: "Email..."

                    Text {
                        anchors.fill: parent
                        text: parent.placeholderText
                        color: "#222"
                        visible: !parent.text
                    }
                }
            }

            Rectangle {
                Layout.alignment: Qt.AlignCenter

                color: "white"
                Layout.preferredWidth: 340
                Layout.preferredHeight: 40
                radius: 20

                TextInput {
                    id: password
                    anchors.fill: parent
                    anchors.margins: 10
                    anchors.leftMargin: 20
                    anchors.rightMargin: 20

                    width: parent.implicitWidth

                    property string placeholderText: "Password..."

                    echoMode: TextInput.Password

                    Text {
                        anchors.fill: parent
                        text: parent.placeholderText
                        color: "#222"
                        visible: !parent.text
                    }
                }
            }

            Rectangle {
                Layout.alignment: Qt.AlignCenter

                color: "#25d666"
                Layout.preferredWidth: 140
                Layout.preferredHeight: 40
                radius: 20

                Text {
                    anchors.centerIn: parent
                    text: "Connect"
                    color: "#222"
                    visible: !parent.text
                }
            }
            Item {
                Layout.fillWidth: true
                Layout.fillHeight: true
            }
        }
    }
}