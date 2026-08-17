import SwiftUI

@main
struct InseamApp: App {
    // The model lives at app scope so the main window and the Settings
    // scene share the one node handle: a Settings save reopens the node
    // the window is querying.
    @StateObject private var model = AppModel()

    var body: some Scene {
        WindowGroup {
            ContentView()
                .environmentObject(model)
        }
        .windowResizability(.contentSize)

        Settings {
            SettingsView()
                .environmentObject(model)
        }
    }
}
