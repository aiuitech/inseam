import ContactsUI
import SwiftUI

/// The system contact picker, yielding the chosen contact's first phone
/// number (or their name when they have none) as the call's participant.
/// The picker runs out of process, so no Contacts permission is needed
/// for this one choice.
struct ContactPicker: UIViewControllerRepresentable {
    let onPick: (String) -> Void

    func makeUIViewController(context: Context) -> CNContactPickerViewController {
        let picker = CNContactPickerViewController()
        picker.displayedPropertyKeys = [CNContactPhoneNumbersKey]
        picker.delegate = context.coordinator
        return picker
    }

    func updateUIViewController(_ controller: CNContactPickerViewController, context: Context) {}

    func makeCoordinator() -> Coordinator {
        Coordinator(onPick: onPick)
    }

    final class Coordinator: NSObject, CNContactPickerDelegate {
        let onPick: (String) -> Void

        init(onPick: @escaping (String) -> Void) {
            self.onPick = onPick
        }

        func contactPicker(_ picker: CNContactPickerViewController, didSelect contact: CNContact) {
            let number = contact.phoneNumbers.first?.value.stringValue
            let name = [contact.givenName, contact.familyName].filter { !$0.isEmpty }.joined(separator: " ")
            onPick(number ?? name)
        }

        func contactPicker(_ picker: CNContactPickerViewController, didSelect contactProperty: CNContactProperty) {
            if let number = contactProperty.value as? CNPhoneNumber {
                onPick(number.stringValue)
            }
        }
    }
}
