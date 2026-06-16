// Subclass fixture — inherits Animal, conforms to Speakable.
// Verifies: IsImplementation(Animal#) + IsImplementation(Speakable/).
#import "Animal.h"
#import "Speakable.h"

@interface Dog : Animal <Speakable>
@property (nonatomic, assign) float volume;
- (instancetype)initWithName:(NSString *)name volume:(float)volume;
@end
