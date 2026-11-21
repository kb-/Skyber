#include "skyber_desktop_bridge/src/bridge.cxxqt.h"

#include <QtGui/QGuiApplication>
#include <QtQml/QQmlApplicationEngine>

int main(int argc, char* argv[]) {
    QGuiApplication app(argc, argv);
    skyber::SessionStatus status;
    QQmlApplicationEngine engine;
    engine.setInitialProperties({{QStringLiteral("callActive"), status.property("callActive")}});
    engine.loadFromModule(QStringLiteral("Skyber"), QStringLiteral("Main"));
    if (engine.rootObjects().isEmpty()) {
        return 1;
    }
    return app.arguments().contains(QStringLiteral("--smoke-test")) ? 0 : app.exec();
}
